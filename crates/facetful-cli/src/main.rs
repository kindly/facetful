//! `facetful` CLI. M1 spike scope:
//!   facetful convert in.csv out.facetful [--row-group-size N]
//!   facetful inspect file.facetful
//!
//! Type inference: all-int64 -> Int64, all-float -> Float64, else Utf8;
//! Utf8 goes dictionary-encoded when distinct values fit u16 and cardinality
//! is below half the row count. Zero dependencies (CSV parsing hand-rolled —
//! quotes, escaped quotes, CRLF).

use facetful_format as fmt;
use fmt::write::{ColumnChunk, SegmentData, Writer};
use fmt::{ColumnDef, ColumnType, Schema, Stats};
use std::collections::HashMap;
use std::process::exit;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("convert") => convert(&args[1..]),
        Some("inspect") => inspect(&args[1..]),
        _ => {
            eprintln!("usage: facetful convert in.csv out.facetful [--row-group-size N]");
            eprintln!("       facetful inspect file.facetful");
            exit(2);
        }
    }
}

// ---------------- CSV ----------------

fn parse_csv(data: &str) -> (Vec<String>, Vec<Vec<String>>) {
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut in_quotes = false;
    let mut chars = data.chars().peekable();
    while let Some(c) = chars.next() {
        if in_quotes {
            match c {
                '"' => {
                    if chars.peek() == Some(&'"') {
                        chars.next();
                        field.push('"');
                    } else {
                        in_quotes = false;
                    }
                }
                _ => field.push(c),
            }
        } else {
            match c {
                '"' => in_quotes = true,
                ',' => row.push(std::mem::take(&mut field)),
                '\r' => {}
                '\n' => {
                    row.push(std::mem::take(&mut field));
                    rows.push(std::mem::take(&mut row));
                }
                _ => field.push(c),
            }
        }
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    let header = rows.remove(0);
    (header, rows)
}

// ---------------- inference ----------------

enum Typed {
    Int(Vec<i64>),
    Float(Vec<f64>),
    Dict { codes: Vec<u16>, dict: Vec<String> },
    Text(Vec<String>),
}

fn infer_column(values: Vec<String>) -> Typed {
    let mut all_int = true;
    let mut all_float = true;
    for v in &values {
        if all_int && v.parse::<i64>().is_err() {
            all_int = false;
        }
        if all_float && v.parse::<f64>().is_err() {
            all_float = false;
        }
        if !all_int && !all_float {
            break;
        }
    }
    if all_int {
        return Typed::Int(values.iter().map(|v| v.parse().unwrap()).collect());
    }
    if all_float {
        return Typed::Float(values.iter().map(|v| v.parse().unwrap()).collect());
    }
    // distinct count for dictionary decision
    let mut index: HashMap<String, u16> = HashMap::new();
    let mut dict: Vec<String> = Vec::new();
    let mut codes: Vec<u16> = Vec::with_capacity(values.len());
    for v in &values {
        if let Some(&c) = index.get(v) {
            codes.push(c);
        } else {
            if dict.len() >= u16::MAX as usize {
                return Typed::Text(values);
            }
            let c = dict.len() as u16;
            dict.push(v.clone());
            index.insert(v.clone(), c);
            codes.push(c);
        }
    }
    if dict.len() * 2 < values.len() {
        Typed::Dict { codes, dict }
    } else {
        Typed::Text(values)
    }
}

fn utf8_offsets(strings: &[String]) -> (Vec<u32>, Vec<u8>) {
    let mut offsets = Vec::with_capacity(strings.len() + 1);
    let mut bytes = Vec::new();
    offsets.push(0u32);
    for s in strings {
        bytes.extend_from_slice(s.as_bytes());
        offsets.push(bytes.len() as u32);
    }
    (offsets, bytes)
}

// ---------------- convert ----------------

fn convert(args: &[String]) {
    let (mut input, mut output, mut group_size) = (None, None, 65536usize);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--row-group-size" => {
                group_size = it.next().expect("--row-group-size needs a value").parse().unwrap()
            }
            _ if input.is_none() => input = Some(a.clone()),
            _ if output.is_none() => output = Some(a.clone()),
            _ => {
                eprintln!("unexpected argument {a}");
                exit(2);
            }
        }
    }
    let (input, output) = match (input, output) {
        (Some(i), Some(o)) => (i, o),
        _ => {
            eprintln!("usage: facetful convert in.csv out.facetful [--row-group-size N]");
            exit(2);
        }
    };

    let data = std::fs::read_to_string(&input).unwrap_or_else(|e| {
        eprintln!("cannot read {input}: {e}");
        exit(1);
    });
    let (header, rows) = parse_csv(&data);
    let nrows = rows.len();
    let ncols = header.len();
    eprintln!("{input}: {nrows} rows, {ncols} columns");

    // column-major
    let mut cols: Vec<Vec<String>> = vec![Vec::with_capacity(nrows); ncols];
    for r in rows {
        assert_eq!(r.len(), ncols, "ragged CSV row");
        for (i, v) in r.into_iter().enumerate() {
            cols[i].push(v);
        }
    }

    let typed: Vec<Typed> = cols.into_iter().map(infer_column).collect();
    let schema = Schema {
        columns: header
            .iter()
            .zip(&typed)
            .map(|(name, t)| {
                let (ty, flags) = match t {
                    Typed::Int(_) => (ColumnType::Int64, 0),
                    Typed::Float(_) => (ColumnType::Float64, 0),
                    Typed::Dict { .. } => (ColumnType::Utf8, fmt::flags::DICTIONARY),
                    Typed::Text(_) => (ColumnType::Utf8, 0),
                };
                ColumnDef { name: name.clone(), ty, flags }
            })
            .collect(),
    };
    for (c, t) in schema.columns.iter().zip(&typed) {
        let kind = match t {
            Typed::Int(_) => "int64".into(),
            Typed::Float(_) => "float64".into(),
            Typed::Dict { dict, .. } => format!("utf8/dict[{}]", dict.len()),
            Typed::Text(_) => "utf8".into(),
        };
        eprintln!("  {}: {kind}", c.name);
    }

    let mut w = Writer::new(schema, vec![], group_size as u32);
    let mut start = 0;
    while start < nrows {
        let rows_here = group_size.min(nrows - start);
        let end = start + rows_here;
        // Build owned per-group buffers first, then borrow for the writer call.
        let owned: Vec<OwnedChunk> = typed
            .iter()
            .map(|t| match t {
                Typed::Int(v) => OwnedChunk::Fixed(v[start..end].iter().flat_map(|x| x.to_le_bytes()).collect()),
                Typed::Float(v) => OwnedChunk::Fixed(v[start..end].iter().flat_map(|x| x.to_le_bytes()).collect()),
                Typed::Dict { codes, dict } => {
                    let (doff, dbytes) = utf8_offsets(dict);
                    OwnedChunk::Dict { codes: codes[start..end].to_vec(), doff, dbytes }
                }
                Typed::Text(v) => {
                    let (off, bytes) = utf8_offsets(&v[start..end]);
                    OwnedChunk::Utf8 { off, bytes }
                }
            })
            .collect();
        let chunks: Vec<ColumnChunk> = owned
            .iter()
            .map(|o| ColumnChunk {
                data: match o {
                    OwnedChunk::Fixed(b) => SegmentData::Fixed(b),
                    OwnedChunk::Dict { codes, doff, dbytes } => {
                        SegmentData::Dict { codes, dict_offsets: doff, dict_bytes: dbytes }
                    }
                    OwnedChunk::Utf8 { off, bytes } => SegmentData::Utf8 { offsets: off, bytes },
                },
                validity: None,
                null_count: 0,
            })
            .collect();
        w.write_group(rows_here as u32, &chunks);
        start = end;
    }
    let bytes = w.finish();
    std::fs::write(&output, &bytes).unwrap();
    eprintln!("{output}: {} bytes ({} row groups)", bytes.len(), nrows.div_ceil(group_size));
}

enum OwnedChunk {
    Fixed(Vec<u8>),
    Dict { codes: Vec<u16>, doff: Vec<u32>, dbytes: Vec<u8> },
    Utf8 { off: Vec<u32>, bytes: Vec<u8> },
}

// ---------------- inspect ----------------

fn inspect(args: &[String]) {
    let path = args.first().unwrap_or_else(|| {
        eprintln!("usage: facetful inspect file.facetful");
        exit(2);
    });
    let bytes = std::fs::read(path).unwrap_or_else(|e| {
        eprintln!("cannot read {path}: {e}");
        exit(1);
    });
    let src: &[u8] = &bytes;
    let cat = fmt::read::open(&src).unwrap_or_else(|e| {
        eprintln!("{path}: {e}");
        exit(1);
    });
    println!("{path}: format v{}, {} rows, {} row groups (target {})",
        cat.version, cat.total_rows, cat.groups.len(), cat.row_group_target);
    if !cat.sorted_by.is_empty() {
        let keys: Vec<String> = cat.sorted_by.iter()
            .map(|k| format!("{}{}", cat.schema.columns[k.column as usize].name, if k.descending { " desc" } else { "" }))
            .collect();
        println!("sorted by: {}", keys.join(", "));
    }
    for (i, c) in cat.schema.columns.iter().enumerate() {
        let bytes_total: u64 = cat.groups.iter().map(|g| g.cols[i].seg_lens.iter().map(|&l| l as u64).sum::<u64>()).sum();
        let stats = cat.groups.iter().map(|g| &g.cols[i].stats).fold(String::new(), |acc, s| {
            if !acc.is_empty() { return acc; }
            match s {
                Stats::Int { min, max } => format!("min {min}, max {max} (group 0)"),
                Stats::Float { min, max } => format!("min {min}, max {max} (group 0)"),
                Stats::None => String::new(),
            }
        });
        println!("  {:20} {:?}{} {:>10} bytes  {}", c.name, c.ty, if c.is_dict() { " dict" } else { "" }, bytes_total, stats);
    }
}
