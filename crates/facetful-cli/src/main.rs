//! `facetful` CLI. M1 spike scope:
//!   facetful convert in.csv out.facetful [--row-group-size N]
//!   facetful inspect file.facetful
//!
//! Type inference: all-int64 -> Int64, all-float -> Float64, else Utf8;
//! Utf8 goes dictionary-encoded when distinct values fit u16 and cardinality
//! is below half the row count. Zero dependencies (CSV parsing hand-rolled —
//! quotes, escaped quotes, CRLF).

use facetful_format as fmt;
use fmt::write::{ColumnChunk, DictData, SegmentData, Writer};
use fmt::{ColumnDef, ColumnType, Schema, Stats};
use std::collections::HashMap;
use std::process::exit;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("convert") => convert(&args[1..]),
        Some("inspect") => inspect(&args[1..]),
        Some("query") => query(&args[1..]),
        _ => {
            eprintln!("usage: facetful convert in.csv out.facetful [--row-group-size N]");
            eprintln!("       facetful inspect file.facetful");
            eprintln!("       facetful query file.facetful [\"select …\"]   (no SQL = REPL)");
            exit(2);
        }
    }
}

// ---------------- query / REPL ----------------

fn query(args: &[String]) {
    use facetful_engine::sql::exec::Val;
    use facetful_engine::sql::run_query;
    use facetful_engine::Table;
    use std::io::{BufRead, Write};

    let Some(path) = args.first() else {
        eprintln!("usage: facetful query file.facetful [\"select …\"]");
        exit(2);
    };
    let bytes = std::fs::read(path).unwrap_or_else(|e| {
        eprintln!("cannot read {path}: {e}");
        exit(1);
    });
    let mut table = Table::open(bytes).unwrap_or_else(|e| {
        eprintln!("{path}: {e}");
        exit(1);
    });

    let render = |v: &Val| -> String {
        match v {
            Val::Null => "".into(),
            Val::Bool(b) => b.to_string(),
            Val::Int(i) => i.to_string(),
            Val::Float(f) => {
                if f.fract() == 0.0 { format!("{f:.1}") } else { format!("{f}") }
            }
            Val::Text(s) => s.to_string(),
        }
    };

    let one_shot = args.get(1).cloned();
    if one_shot.is_none() {
        let cat = table.catalog();
        eprintln!(
            "facetful query — {} rows, {} columns. SQL at the prompt; empty line or ctrl-d quits.",
            cat.total_rows,
            cat.schema.columns.len()
        );
        let names: Vec<&str> = cat.schema.columns.iter().map(|c| c.name.as_str()).collect();
        eprintln!("columns: {} (table name: t)", names.join(", "));
    }

    let mut run_one = |sql: &str| {
        let t0 = std::time::Instant::now();
        match run_query(&mut table, sql) {
            Err(d) => eprint!("{}", d.render(sql)),
            Ok(r) => {
                let mut widths: Vec<usize> = r.columns.iter().map(|c| c.len()).collect();
                let cells: Vec<Vec<String>> = r
                    .rows
                    .iter()
                    .map(|row| {
                        row.iter()
                            .enumerate()
                            .map(|(i, v)| {
                                let s = render(v);
                                widths[i] = widths[i].max(s.len());
                                s
                            })
                            .collect()
                    })
                    .collect();
                let line = |cols: &[String]| {
                    cols.iter()
                        .enumerate()
                        .map(|(i, c)| format!("{c:<w$}", w = widths[i]))
                        .collect::<Vec<_>>()
                        .join("  ")
                };
                println!("{}", line(&r.columns));
                println!("{}", widths.iter().map(|w| "-".repeat(*w)).collect::<Vec<_>>().join("  "));
                for row in &cells {
                    println!("{}", line(row));
                }
                let pruned = r.total_groups - r.scanned_groups;
                println!(
                    "({} row{}, {:.1} ms{})",
                    cells.len(),
                    if cells.len() == 1 { "" } else { "s" },
                    t0.elapsed().as_secs_f64() * 1000.0,
                    if pruned > 0 {
                        format!(", skipped {pruned}/{} row groups", r.total_groups)
                    } else {
                        String::new()
                    }
                );
            }
        }
    };

    if let Some(arg) = one_shot {
        if arg == "--bench" {
            // benchmark mode: file of `# name` + SQL blocks; median of measured runs
            let qfile = args.get(2).expect("--bench needs a queries file");
            let text = std::fs::read_to_string(qfile).unwrap();
            let mut blocks: Vec<(String, String)> = Vec::new();
            for chunk in text.split('#').skip(1) {
                let (name, sql) = chunk.split_once('\n').unwrap_or((chunk, ""));
                let sql = sql.trim();
                if !sql.is_empty() {
                    blocks.push((name.trim().to_string(), sql.to_string()));
                }
            }
            for (name, sql) in &blocks {
                use facetful_engine::sql::run_query;
                // warmup
                for _ in 0..3 {
                    if let Err(d) = run_query(&mut table, sql) {
                        eprint!("{name}: {}", d.render(sql));
                        std::process::exit(1);
                    }
                }
                let mut times: Vec<f64> = (0..10)
                    .map(|_| {
                        let t0 = std::time::Instant::now();
                        let _ = run_query(&mut table, sql).unwrap();
                        t0.elapsed().as_secs_f64() * 1000.0
                    })
                    .collect();
                times.sort_by(|a, b| a.partial_cmp(b).unwrap());
                println!("{name}\t{:.2}", times[times.len() / 2]);
            }
            return;
        }
        run_one(&arg);
        return;
    }

    // REPL
    let stdin = std::io::stdin();
    loop {
        eprint!("facetful> ");
        std::io::stderr().flush().ok();
        let mut linebuf = String::new();
        match stdin.lock().read_line(&mut linebuf) {
            Ok(0) => break,
            Ok(_) => {
                let sql = linebuf.trim();
                if sql.is_empty() {
                    break;
                }
                run_one(sql);
            }
            Err(_) => break,
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
    /// Integers plus the narrowest type that fits; valid[i] = false means null
    /// (empty CSV cell) and the value is a 0 placeholder.
    Int(Vec<i64>, ColumnType, Option<Vec<bool>>),
    Float(Vec<f64>, Option<Vec<bool>>),
    Dict { codes: Vec<u16>, dict: Vec<String> },
    Text(Vec<String>),
}

fn narrowest_int(min: i64, max: i64) -> ColumnType {
    if min >= i8::MIN as i64 && max <= i8::MAX as i64 {
        ColumnType::Int8
    } else if min >= i16::MIN as i64 && max <= i16::MAX as i64 {
        ColumnType::Int16
    } else if min >= i32::MIN as i64 && max <= i32::MAX as i64 {
        ColumnType::Int32
    } else {
        ColumnType::Int64
    }
}

fn infer_column(values: Vec<String>) -> Typed {
    // Empty cells are nulls for numeric columns; for string columns the empty
    // string is just a dictionary value (facet UIs show a "(blank)" bucket).
    let mut all_int = true;
    let mut all_float = true;
    let mut non_empty = 0usize;
    for v in &values {
        if v.is_empty() {
            continue;
        }
        non_empty += 1;
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
    let has_nulls = non_empty < values.len();
    let valids = || -> Option<Vec<bool>> {
        if has_nulls { Some(values.iter().map(|v| !v.is_empty()).collect()) } else { None }
    };
    if all_int && non_empty > 0 {
        let ints: Vec<i64> =
            values.iter().map(|v| if v.is_empty() { 0 } else { v.parse().unwrap() }).collect();
        let present = ints.iter().zip(&values).filter(|(_, v)| !v.is_empty());
        let min = present.clone().map(|(i, _)| *i).min().unwrap_or(0);
        let max = present.map(|(i, _)| *i).max().unwrap_or(0);
        return Typed::Int(ints, narrowest_int(min, max), valids());
    }
    if all_float && non_empty > 0 {
        let floats: Vec<f64> =
            values.iter().map(|v| if v.is_empty() { 0.0 } else { v.parse().unwrap() }).collect();
        return Typed::Float(floats, valids());
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
                    Typed::Int(_, ty, _) => (*ty, 0),
                    Typed::Float(_, _) => (ColumnType::Float64, 0),
                    Typed::Dict { dict, .. } => (
                        ColumnType::Utf8,
                        fmt::flags::DICTIONARY
                            | if dict.len() <= 256 { fmt::flags::CODES_U8 } else { 0 },
                    ),
                    Typed::Text(_) => (ColumnType::Utf8, 0),
                };
                ColumnDef { name: name.clone(), ty, flags }
            })
            .collect(),
    };
    for (c, t) in schema.columns.iter().zip(&typed) {
        let kind = match t {
            Typed::Int(_, ty, nulls) => format!(
                "{}{}",
                format!("{ty:?}").to_lowercase(),
                if nulls.is_some() { " (nullable)" } else { "" }
            ),
            Typed::Float(_, nulls) => {
                format!("float64{}", if nulls.is_some() { " (nullable)" } else { "" })
            }
            Typed::Dict { dict, .. } => format!(
                "utf8/dict[{}] (u{} codes)",
                dict.len(),
                c.code_width() * 8
            ),
            Typed::Text(_) => "utf8".into(),
        };
        eprintln!("  {}: {kind}", c.name);
    }

    let dicts: Vec<Option<DictData>> = typed
        .iter()
        .map(|t| match t {
            Typed::Dict { dict, .. } => {
                let (offsets, bytes) = utf8_offsets(dict);
                Some(DictData { offsets, bytes })
            }
            _ => None,
        })
        .collect();
    let mut w = Writer::new(schema.clone(), vec![], group_size as u32, &dicts);
    let mut start = 0;
    while start < nrows {
        let rows_here = group_size.min(nrows - start);
        let end = start + rows_here;
        // Build owned per-group buffers first, then borrow for the writer call.
        let owned: Vec<OwnedChunk> = typed
            .iter()
            .zip(&schema.columns)
            .map(|(t, def)| match t {
                Typed::Int(v, ty, nulls) => OwnedChunk::Fixed(
                    match ty {
                        ColumnType::Int8 => v[start..end].iter().map(|&x| x as i8 as u8).collect(),
                        ColumnType::Int16 => v[start..end].iter().flat_map(|&x| (x as i16).to_le_bytes()).collect(),
                        ColumnType::Int32 => v[start..end].iter().flat_map(|&x| (x as i32).to_le_bytes()).collect(),
                        _ => v[start..end].iter().flat_map(|x| x.to_le_bytes()).collect(),
                    },
                    validity_bitmap(nulls, start, end),
                ),
                Typed::Float(v, nulls) => OwnedChunk::Fixed(
                    v[start..end].iter().flat_map(|x| x.to_le_bytes()).collect(),
                    validity_bitmap(nulls, start, end),
                ),
                Typed::Dict { codes, .. } => {
                    if def.code_width() == 1 {
                        OwnedChunk::Codes8(codes[start..end].iter().map(|&c| c as u8).collect())
                    } else {
                        OwnedChunk::Codes16(codes[start..end].to_vec())
                    }
                }
                Typed::Text(v) => {
                    let (off, bytes) = utf8_offsets(&v[start..end]);
                    OwnedChunk::Utf8 { off, bytes }
                }
            })
            .collect();
        let chunks: Vec<ColumnChunk> = owned
            .iter()
            .map(|o| {
                let (data, validity) = match o {
                    OwnedChunk::Fixed(b, v) => (SegmentData::Fixed(b), v.as_ref()),
                    OwnedChunk::Codes8(c) => (SegmentData::Codes8(c), None),
                    OwnedChunk::Codes16(c) => (SegmentData::Codes16(c), None),
                    OwnedChunk::Utf8 { off, bytes } => {
                        (SegmentData::Utf8 { offsets: off, bytes }, None)
                    }
                };
                let null_count = validity
                    .map(|(_bits, nulls)| *nulls)
                    .unwrap_or(0);
                ColumnChunk {
                    data,
                    validity: validity.map(|(bits, _)| bits.as_slice()),
                    null_count,
                }
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
    /// data bytes + optional (validity bitmap, null count) for this group slice
    Fixed(Vec<u8>, Option<(Vec<u8>, u32)>),
    Codes8(Vec<u8>),
    Codes16(Vec<u16>),
    Utf8 { off: Vec<u32>, bytes: Vec<u8> },
}

/// Bitmap for rows [start, end) of a column's valid flags; None if that slice
/// has no nulls.
fn validity_bitmap(valids: &Option<Vec<bool>>, start: usize, end: usize) -> Option<(Vec<u8>, u32)> {
    let valids = valids.as_ref()?;
    let slice = &valids[start..end];
    let nulls = slice.iter().filter(|&&v| !v).count() as u32;
    if nulls == 0 {
        return None;
    }
    let mut bits = vec![0u8; (slice.len() + 7) / 8];
    for (i, &v) in slice.iter().enumerate() {
        if v {
            bits[i / 8] |= 1 << (i % 8);
        }
    }
    Some((bits, nulls))
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
