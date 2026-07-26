//! Serialize-able data-transfer types for the IPC boundary.
//!
//! Core types stay serde-for-IPC-free (STANDARDS §1): the shell owns the
//! `Serialize`/`Deserialize` shapes the webview sees. This module holds those
//! shapes plus `From` conversions from the core types they mirror. Field
//! names are snake_case on both sides — the matching TypeScript interfaces
//! live in `frontend/src/lib/types.ts` and must stay in step.

use emfit_core::model::volume::VolumeInfo;
use emfit_core::service::presets::Preset;
use emfit_core::service::query::RawQuery;
use emfit_core::service::task::Progress;
use emfit_core::service::view::{Row, SelectionSummary, Sort, SortKey, human_size};
use serde::{Deserialize, Serialize};

/// Serializable mirror of [`Progress`] for scan progress events.
///
/// `#[serde(tag = "kind")]` makes each variant an object like
/// `{ "kind": "Tick", "done": 42 }`, which the frontend switches on.
#[derive(Serialize, Debug, Clone)]
#[serde(tag = "kind")]
pub enum ProgressDto {
    /// A phase begins. `message` describes what is happening right now;
    /// `total` is the expected unit count when known.
    Started {
        message: String,
        total: Option<u64>,
    },
    /// One unit of progress. `done` is cumulative, not delta.
    Tick {
        done: u64,
    },
    Finished,
    Failed {
        message: String,
    },
}

impl From<Progress> for ProgressDto {
    fn from(progress: Progress) -> Self {
        match progress {
            Progress::Started { message, total } => ProgressDto::Started { message, total },
            Progress::Tick { done } => ProgressDto::Tick { done },
            Progress::Finished => ProgressDto::Finished,
            Progress::Failed { message } => ProgressDto::Failed { message },
        }
    }
}

/// Payload of the `scan:progress` event: which volume, what happened.
#[derive(Serialize, Debug, Clone)]
pub struct ScanProgressDto {
    pub volume: String,
    #[serde(flatten)]
    pub progress: ProgressDto,
}

/// Payload of the `scan:done` event, one per volume — per-volume failure
/// isolation (features.md §7): one drive failing reports here while the
/// others carry on.
#[derive(Serialize, Debug, Clone)]
pub struct ScanDoneDto {
    pub volume: String,
    pub ok: bool,
    /// Present when `ok` is false.
    pub error: Option<String>,
    pub files: u64,
    pub directories: u64,
    pub total_size: u64,
    pub total_display: String,
    pub elapsed_ms: u64,
}

/// One volume for the drive-selection UI.
#[derive(Serialize, Debug, Clone)]
pub struct VolumeDto {
    /// Display key (`C:`), also the id `start_scan` takes.
    pub name: String,
    pub filesystem: String,
    pub label: Option<String>,
    pub total_bytes: u64,
    pub free_bytes: u64,
    pub total_display: String,
    pub free_display: String,
    /// Whether the fast MFT path can read it (NTFS on one partition).
    pub raw_scannable: bool,
    /// Whether an index for it is currently loaded.
    pub scanned: bool,
}

impl VolumeDto {
    pub fn from_info(info: &VolumeInfo, scanned: bool) -> Self {
        Self {
            name: info.display_name(),
            filesystem: info.filesystem.as_str().to_string(),
            label: info.label.clone(),
            total_bytes: info.total_bytes,
            free_bytes: info.free_bytes,
            total_display: human_size(info.total_bytes),
            free_display: human_size(info.free_bytes),
            raw_scannable: info.supports_raw_scan(),
            scanned,
        }
    }
}

/// The webview's raw query strings, straight into [`RawQuery`]. Parsing and
/// leniency live in the core (`service::query`); the shell just carries.
#[derive(Deserialize, Debug, Clone, Default)]
pub struct RawQueryDto {
    pub text: String,
    pub regex: String,
    pub size: String,
    pub modified: String,
    pub extensions: String,
    pub preset: String,
    pub include_hidden: bool,
    pub include_system: bool,
    pub case_sensitive: Option<bool>,
}

impl From<RawQueryDto> for RawQuery {
    fn from(dto: RawQueryDto) -> Self {
        RawQuery {
            text: dto.text,
            regex: dto.regex,
            size: dto.size,
            modified: dto.modified,
            extensions: dto.extensions,
            preset: dto.preset,
            include_hidden: dto.include_hidden,
            include_system: dto.include_system,
            case_sensitive: dto.case_sensitive,
        }
    }
}

/// Sort request from the column headers.
#[derive(Deserialize, Debug, Clone)]
pub struct SortDto {
    pub key: String,
    pub ascending: bool,
}

impl SortDto {
    /// Unknown keys fall back to the default rather than erroring — a stale
    /// frontend must not wedge the view.
    pub fn to_sort(&self) -> Sort {
        let key = match self.key.as_str() {
            "name" => SortKey::Name,
            "path" => SortKey::Path,
            "size" => SortKey::Size,
            "allocated" => SortKey::Allocated,
            "modified" => SortKey::Modified,
            "extension" => SortKey::Extension,
            "kind" => SortKey::Kind,
            _ => SortKey::default(),
        };
        Sort {
            key,
            ascending: self.ascending,
        }
    }
}

/// One display row (features.md §9: pre-formatted strings plus ids, never
/// full nodes).
#[derive(Serialize, Debug, Clone)]
pub struct RowDto {
    pub vol: u16,
    pub id: u32,
    pub name: String,
    pub dir_path: String,
    pub size: u64,
    pub allocated: u64,
    pub size_display: String,
    pub allocated_display: String,
    pub modified_display: String,
    pub extension: String,
    pub kind_label: String,
    pub category: u8,
    pub is_directory: bool,
    pub is_hidden: bool,
    pub is_system: bool,
    pub is_alias: bool,
    pub is_synthetic: bool,
    pub is_reparse: bool,
}

impl From<Row> for RowDto {
    fn from(row: Row) -> Self {
        Self {
            vol: row.vol,
            id: row.id,
            name: row.name,
            dir_path: row.dir_path,
            size: row.size,
            allocated: row.allocated,
            size_display: row.size_display,
            allocated_display: row.allocated_display,
            modified_display: row.modified_display,
            extension: row.extension,
            kind_label: row.kind_label,
            category: row.category,
            is_directory: row.is_directory,
            is_hidden: row.is_hidden,
            is_system: row.is_system,
            is_alias: row.is_alias,
            is_synthetic: row.is_synthetic,
            is_reparse: row.is_reparse,
        }
    }
}

/// One window of the current view.
#[derive(Serialize, Debug, Clone)]
pub struct RowWindowDto {
    /// The view generation these rows belong to; the frontend discards
    /// windows from a generation it no longer shows.
    pub generation: u64,
    pub total: u64,
    pub offset: u64,
    pub rows: Vec<RowDto>,
}

/// Payload of the `view:updated` event.
#[derive(Serialize, Debug, Clone)]
pub struct ViewUpdatedDto {
    pub generation: u64,
    /// Result count, with its elapsed time (features.md §2).
    pub total: u64,
    pub elapsed_ms: u64,
    /// Query-parse warnings to surface (bad size filter, invalid regex).
    pub warnings: Vec<String>,
    /// Sum of every scanned volume's contents, for the status bar.
    pub volumes_total_bytes: u64,
    pub volumes_total_display: String,
}

/// Selection totals for the status bar ("how much would this free up").
#[derive(Serialize, Debug, Clone, Default)]
pub struct SelectionSummaryDto {
    pub count: u64,
    pub bytes: u64,
    pub allocated: u64,
    pub bytes_display: String,
    pub allocated_display: String,
}

impl From<SelectionSummary> for SelectionSummaryDto {
    fn from(s: SelectionSummary) -> Self {
        Self {
            count: s.count,
            bytes: s.bytes,
            allocated: s.allocated,
            bytes_display: human_size(s.bytes),
            allocated_display: human_size(s.allocated),
        }
    }
}

#[derive(Serialize, Debug, Clone)]
pub struct PresetDto {
    pub name: String,
    pub search: String,
}

impl From<Preset> for PresetDto {
    fn from(p: Preset) -> Self {
        Self {
            name: p.name,
            search: p.search,
        }
    }
}
