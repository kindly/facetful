//! `facetful` CLI. M1 spike scope:
//!   facetful convert in.csv out.facetful [--row-group-size N]
//!   facetful inspect file.facetful
//!
//! Type inference: all-int64 -> Int64, all-float -> Float64, else Utf8;
//! Utf8 goes dictionary-encoded when distinct values fit u16 and cardinality
//! is below half the row count. Zero dependencies (CSV parsing hand-rolled —
//! quotes, escaped quotes, CRLF).

use facetful_format as fmt;
use fmt::compile;
use fmt::Stats;
use std::process::exit;

// glibc trims the heap back to the OS between queries, so every query
// re-faults its vector pages — measured 2x on scan-heavy shapes. Pin the
// thresholds up front, like sqlite/duckdb do with their own buffer managers.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn retain_heap() {
    extern "C" {
        fn mallopt(param: core::ffi::c_int, value: core::ffi::c_int) -> core::ffi::c_int;
    }
    const M_TRIM_THRESHOLD: i32 = -1;
    const M_MMAP_THRESHOLD: i32 = -3;
    unsafe {
        mallopt(M_TRIM_THRESHOLD, i32::MAX);
        mallopt(M_MMAP_THRESHOLD, 256 * 1024 * 1024);
    }
}
#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn retain_heap() {}

fn main() {
    retain_heap();
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

    // --mask-cache <bytes> may appear anywhere; 0 disables the filter cache
    let mut args: Vec<String> = args.to_vec();
    let mut mask_budget: Option<usize> = None;
    if let Some(i) = args.iter().position(|a| a == "--mask-cache") {
        let v = args.get(i + 1).unwrap_or_else(|| {
            eprintln!("--mask-cache needs a byte count (0 disables)");
            exit(2);
        });
        mask_budget = Some(v.parse().unwrap_or_else(|_| {
            eprintln!("--mask-cache: '{v}' is not a byte count");
            exit(2);
        }));
        args.drain(i..=i + 1);
    }

    let Some(path) = args.first() else {
        eprintln!("usage: facetful query file.facetful [\"select …\"] [--mask-cache <bytes>]");
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
    if let Some(b) = mask_budget {
        table.masks().set_budget(b);
    }

    use facetful_engine::sql::binder::Ty;
    let render = |v: &Val, ty: Ty| -> String {
        match (v, ty) {
            (Val::Null, _) => "".into(),
            (Val::Int(d), Ty::Date) => fmt::time::format_date(*d),
            (Val::Int(ms), Ty::Timestamp) => fmt::time::format_timestamp(*ms),
            (Val::Bool(b), _) => b.to_string(),
            (Val::Int(i), _) => i.to_string(),
            (Val::Float(f), _) => {
                if f.fract() == 0.0 { format!("{f:.1}") } else { format!("{f}") }
            }
            (Val::Text(s), _) => s.to_string(),
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
            Ok(mut r) => {
                r.ensure_rows();
                let mut widths: Vec<usize> = r.columns.iter().map(|c| c.len()).collect();
                let cells: Vec<Vec<String>> = r
                    .rows
                    .iter()
                    .map(|row| {
                        row.iter()
                            .enumerate()
                            .map(|(i, v)| {
                                let s = render(v, r.col_types[i]);
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
            println!("# query\tcold_ms\twarm_ms");
            for (name, sql) in &blocks {
                use facetful_engine::sql::run_query;
                // warmup: lazy segment loads, dict caches, mask cache
                for _ in 0..3 {
                    if let Err(d) = run_query(&mut table, sql) {
                        eprint!("{name}: {}", d.render(sql));
                        std::process::exit(1);
                    }
                }
                let median = |mut ts: Vec<f64>| {
                    ts.sort_by(|a, b| a.partial_cmp(b).unwrap());
                    ts[ts.len() / 2]
                };
                // warm: mask cache primed by the warmups (facet-refresh shape)
                let warm = median(
                    (0..10)
                        .map(|_| {
                            let t0 = std::time::Instant::now();
                            let _ = run_query(&mut table, sql).unwrap();
                            t0.elapsed().as_secs_f64() * 1000.0
                        })
                        .collect(),
                );
                // cold: every run re-evaluates its WHERE (first-interaction shape);
                // segments/dicts stay warm — this isolates filter-evaluation cost
                let cold = median(
                    (0..10)
                        .map(|_| {
                            table.masks().clear();
                            let t0 = std::time::Instant::now();
                            let _ = run_query(&mut table, sql).unwrap();
                            t0.elapsed().as_secs_f64() * 1000.0
                        })
                        .collect(),
                );
                table.masks().clear(); // don't hand the next query a primed cache
                println!("{name}\t{cold:.2}\t{warm:.2}");
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

/// String cells -> a typed input column for the shared baseline compiler.
/// Empty cells are nulls for numeric columns; for string columns the empty
/// string is just a value (facet UIs show a "(blank)" bucket).
fn infer_column(values: Vec<String>) -> compile::InCol {
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
        let v = values.iter().map(|s| if s.is_empty() { 0 } else { s.parse().unwrap() }).collect();
        return compile::InCol::Int { v, valid: valids() };
    }
    if all_float && non_empty > 0 {
        let v =
            values.iter().map(|s| if s.is_empty() { 0.0 } else { s.parse().unwrap() }).collect();
        return compile::InCol::Float { v, valid: valids() };
    }
    // ISO dates ("YYYY-MM-DD") / datetimes -> real temporal columns
    if non_empty > 0 && values.iter().all(|s| s.is_empty() || fmt::time::parse_date(s).is_some()) {
        let v = values
            .iter()
            .map(|s| if s.is_empty() { 0 } else { fmt::time::parse_date(s).unwrap() as i32 })
            .collect();
        return compile::InCol::Date { v, valid: valids() };
    }
    if non_empty > 0
        && values.iter().all(|s| s.is_empty() || fmt::time::parse_timestamp(s).is_some())
    {
        let v = values
            .iter()
            .map(|s| if s.is_empty() { 0 } else { fmt::time::parse_timestamp(s).unwrap() })
            .collect();
        return compile::InCol::Timestamp { v, valid: valids() };
    }
    compile::InCol::Text { v: values, valid: None }
}

// ---------------- convert ----------------

fn convert(args: &[String]) {
    let (mut input, mut output, mut group_size) = (None, None, 65536u32);
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

    let in_cols: Vec<compile::InCol> = cols.into_iter().map(infer_column).collect();
    let (bytes, schema) = compile::compile(&header, in_cols, group_size).unwrap_or_else(|e| {
        eprintln!("compile failed: {e}");
        exit(1);
    });
    for (c, kind) in schema.columns.iter().zip(compile::describe(&schema)) {
        eprintln!("  {}: {kind}", c.name);
    }
    std::fs::write(&output, &bytes).unwrap();
    eprintln!(
        "{output}: {} bytes ({} row groups)",
        bytes.len(),
        (nrows as u32).div_ceil(group_size)
    );
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
