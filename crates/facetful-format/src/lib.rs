//! The `.facetful` columnar file format (v1).
//!
//! Layout (see docs/design.sv for the rationale):
//! ```text
//! [header: magic "FCT1", header_len, version, row-group target, schema, sorted_by]
//! [row-group 0: group header (row count, per-column null counts + segment lengths) + segments…]
//! …
//! [footer: total rows, group directory (offsets, row counts, per-column lens + min/max stats)]
//! [footer_len: u32 LE] [magic "FCT1"]
//! ```
//! Readable from both ends: the header + self-framing groups serve streaming readers;
//! the footer serves random access (OPFS, HTTP range). Every segment is padded to
//! 8-byte alignment so a loaded segment can be viewed as its native type directly —
//! the on-disk bytes of a segment ARE the in-memory representation (zero-decode).
//! No serde anywhere: the codec is hand-rolled little-endian.

pub mod read;
pub mod write;

pub const MAGIC: [u8; 4] = *b"FCT1";
pub const VERSION: u16 = 1;
/// Max segments a single column contributes to one row group
/// (dict columns: codes + dict offsets + dict bytes).
pub const MAX_SEGS: usize = 3;
pub const ALIGN: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ColumnType {
    Bool = 0,
    Int8 = 1,
    Int16 = 2,
    Int32 = 3,
    Int64 = 4,
    Float64 = 5,
    Utf8 = 6,
    /// Days since epoch, stored as Int32
    Date = 7,
    /// Milliseconds since epoch, stored as Int64
    Timestamp = 8,
}

impl ColumnType {
    pub fn from_tag(t: u8) -> Option<Self> {
        Some(match t {
            0 => Self::Bool,
            1 => Self::Int8,
            2 => Self::Int16,
            3 => Self::Int32,
            4 => Self::Int64,
            5 => Self::Float64,
            6 => Self::Utf8,
            7 => Self::Date,
            8 => Self::Timestamp,
            _ => return None,
        })
    }

    /// Byte width of one value for fixed-width types (Bool is bit-packed: None).
    pub fn fixed_width(self) -> Option<usize> {
        Some(match self {
            Self::Int8 => 1,
            Self::Int16 => 2,
            Self::Int32 | Self::Date => 4,
            Self::Int64 | Self::Timestamp | Self::Float64 => 8,
            Self::Bool | Self::Utf8 => return None,
        })
    }
}

/// Column flag bits (u16). Reserved bits must be zero in v1 readers.
pub mod flags {
    /// Utf8 column is dictionary-encoded: codes segment + dict offsets + dict bytes.
    pub const DICTIONARY: u16 = 1 << 0;
    /// Reserved: per-segment compression (not implemented in v1).
    pub const COMPRESSED: u16 = 1 << 1;
    pub const KNOWN: u16 = DICTIONARY;
}

#[derive(Debug, Clone)]
pub struct ColumnDef {
    pub name: String,
    pub ty: ColumnType,
    pub flags: u16,
}

impl ColumnDef {
    pub fn is_dict(&self) -> bool {
        self.flags & flags::DICTIONARY != 0
    }
}

#[derive(Debug, Clone)]
pub struct Schema {
    pub columns: Vec<ColumnDef>,
}

#[derive(Debug, Clone, Copy)]
pub struct SortKey {
    pub column: u16,
    pub descending: bool,
}

/// Per-segment min/max statistics. Numeric only in v1 (Utf8: `None`).
/// Stored as the 8-byte LE bit pattern of i64 (ints/date/ts) or f64.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Stats {
    None,
    Int { min: i64, max: i64 },
    Float { min: f64, max: f64 },
}

/// Directory entry for one column within one row group.
#[derive(Debug, Clone)]
pub struct ColMeta {
    pub null_count: u32,
    /// Padded on-disk length of each segment; unused slots are 0.
    pub seg_lens: [u32; MAX_SEGS],
    pub stats: Stats,
}

#[derive(Debug, Clone)]
pub struct GroupMeta {
    /// Absolute file offset of the group (its group header).
    pub offset: u64,
    pub row_count: u32,
    pub cols: Vec<ColMeta>,
}

/// Everything a reader needs, parsed once at open. Never kept as raw bytes.
#[derive(Debug, Clone)]
pub struct Catalog {
    pub version: u16,
    pub row_group_target: u32,
    pub schema: Schema,
    pub sorted_by: Vec<SortKey>,
    pub total_rows: u64,
    pub groups: Vec<GroupMeta>,
}

pub fn align_up(n: usize) -> usize {
    (n + ALIGN - 1) & !(ALIGN - 1)
}

/// Number of segments a column occupies in every row group.
pub fn seg_count(col: &ColumnDef) -> usize {
    match (col.ty, col.is_dict()) {
        (ColumnType::Utf8, true) => 3,  // codes, dict offsets, dict bytes
        (ColumnType::Utf8, false) => 2, // offsets, bytes
        _ => 1,
    }
}

#[derive(Debug)]
pub enum FormatError {
    BadMagic,
    UnsupportedVersion(u16),
    UnknownType(u8),
    UnknownFlags(u16),
    Truncated,
    Corrupt(&'static str),
    Io(&'static str),
}

impl core::fmt::Display for FormatError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::BadMagic => write!(f, "not a .facetful file (bad magic)"),
            Self::UnsupportedVersion(v) => write!(f, "unsupported format version {v}"),
            Self::UnknownType(t) => write!(f, "unknown column type tag {t}"),
            Self::UnknownFlags(x) => write!(f, "unknown column flags {x:#x}"),
            Self::Truncated => write!(f, "file truncated"),
            Self::Corrupt(m) => write!(f, "corrupt file: {m}"),
            Self::Io(m) => write!(f, "io error: {m}"),
        }
    }
}
