//! `facetful` CLI. M1 spike scope:
//!   facetful convert in.csv out.facetful [--row-group-size N]
//!   facetful inspect file.facetful
//!
//! Type inference: all-int64 -> Int64, all-float -> Float64, else Utf8;
//! Utf8 goes dictionary-encoded when distinct values fit u16 and cardinality
//! is below half the row count. Zero dependencies (CSV parsing hand-rolled —
//! quotes, escaped quotes, CRLF).

use facetful_format as fmt;

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
        Some("materialize") => materialize(&args[1..]),
        Some("join") => join(&args[1..]),
        _ => {
            eprintln!("usage: facetful convert in.csv out.facetful [--row-group-size N]");
            eprintln!("       facetful inspect file.facetful");
            eprintln!("       facetful query file.facetful [\"select …\"]   (no SQL = REPL)");
            eprintln!("       facetful materialize in.facetful \"select …\" out.facetful [--row-group-size N]");
            eprintln!("       facetful join left.facetful right.facetful out.facetful --on l=r[,l2=r2] [--columns a,b] [--inner]");
            exit(2);
        }
    }
}

// ---------------- query / REPL ----------------

fn query(args: &[String]) {
    use facetful_engine::sql::exec::Val;
    use facetful_engine::sql::run_query_with;
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
    // --table name=path (repeatable): other tables the SQL may name in FROM / JOIN
    let mut others: Vec<(String, String)> = Vec::new();
    while let Some(i) = args.iter().position(|a| a == "--table") {
        let spec = args.get(i + 1).cloned().unwrap_or_else(|| {
            eprintln!("--table needs name=path.facetful");
            exit(2);
        });
        let Some((name, path)) = spec.split_once('=') else {
            eprintln!("--table: '{spec}' is not name=path.facetful");
            exit(2);
        };
        others.push((name.to_string(), path.to_string()));
        args.drain(i..=i + 1);
    }

    let Some(path) = args.first() else {
        eprintln!("usage: facetful query file.facetful [\"select …\"] [--mask-cache <bytes>] [--table name=other.facetful …]");
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
    let mut set = facetful_engine::sql::TableSet { tables: Vec::new() };
    for (name, p) in &others {
        let bytes = std::fs::read(p).unwrap_or_else(|e| {
            eprintln!("cannot read {p}: {e}");
            exit(1);
        });
        let t = Table::open(bytes).unwrap_or_else(|e| {
            eprintln!("{p}: {e}");
            exit(1);
        });
        set.tables.push((name.clone(), t));
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
        match run_query_with(&mut table, sql, &mut set) {
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
                // warmup: lazy segment loads, dict caches, mask cache
                for _ in 0..3 {
                    if let Err(d) = run_query_with(&mut table, sql, &mut set) {
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
                            let _ = run_query_with(&mut table, sql, &mut set).unwrap();
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
                            let _ = run_query_with(&mut table, sql, &mut set).unwrap();
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

/// Two passes over the file through the streaming converter
/// (`facetful_format::stream`, design.sv d51): sniff types and dictionaries,
/// then encode row groups straight to the output. Memory is bounded by the
/// distinct values on dictionary candidates plus one row group, whatever the
/// file size.
fn convert(args: &[String]) {
    use fmt::stream::{CsvReader, Encoder, Sniffer};
    use std::io::{Read, Write};
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
    const CHUNK: usize = 1 << 20;
    // feed a whole file through a CsvReader
    let feed = |path: &str, on_row: &mut dyn FnMut(&[String]) -> Result<(), String>| -> Result<(), String> {
        let mut f = std::fs::File::open(path).map_err(|e| format!("cannot read {path}: {e}"))?;
        let mut reader = CsvReader::new();
        let mut buf = vec![0u8; CHUNK];
        let mut err: Option<String> = None;
        loop {
            let n = f.read(&mut buf).map_err(|e| format!("read {path}: {e}"))?;
            if n == 0 {
                break;
            }
            reader.push(&buf[..n], |row| {
                if err.is_none() {
                    if let Err(e) = on_row(row) {
                        err = Some(e);
                    }
                }
            });
            if let Some(e) = err {
                return Err(e);
            }
        }
        reader.finish(|row| {
            if err.is_none() {
                if let Err(e) = on_row(row) {
                    err = Some(e);
                }
            }
        });
        err.map_or(Ok(()), Err)
    };
    let fail = |e: String| -> ! {
        eprintln!("{e}");
        exit(1)
    };
    // pass 1
    let mut sniffer: Option<Sniffer> = None;
    feed(&input, &mut |row| {
        match &mut sniffer {
            None => sniffer = Some(Sniffer::new(row.to_vec())),
            Some(s) => s.row(row)?,
        }
        Ok(())
    })
    .unwrap_or_else(|e| fail(e));
    let plan = sniffer.unwrap_or_else(|| fail("empty input".into())).finish();
    eprintln!("{input}: {} rows, {} columns", plan.rows, plan.names.len());
    // pass 2
    let mut enc = Encoder::new(plan, group_size).unwrap_or_else(|e| fail(e));
    for (c, kind) in enc.schema().columns.iter().zip(fmt::compile::describe(enc.schema())) {
        eprintln!("  {}: {kind}", c.name);
    }
    let mut out = std::fs::File::create(&output).unwrap_or_else(|e| fail(format!("cannot write {output}: {e}")));
    let mut written = 0u64;
    let mut first = true;
    feed(&input, &mut |row| {
        if first {
            first = false;
            return Ok(());
        }
        enc.row(row)?;
        let bytes = enc.take_output();
        if !bytes.is_empty() {
            out.write_all(&bytes).map_err(|e| format!("write {output}: {e}"))?;
            written += bytes.len() as u64;
        }
        Ok(())
    })
    .unwrap_or_else(|e| fail(e));
    let rows = enc.rows();
    let tail = enc.finish().unwrap_or_else(|e| fail(e));
    out.write_all(&tail).unwrap_or_else(|e| fail(format!("write {output}: {e}")));
    written += tail.len() as u64;
    eprintln!("{output}: {written} bytes ({} row groups)", (rows as u32).div_ceil(group_size));
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


/// `facetful materialize in.facetful "sql" out.facetful [--row-group-size N]`:
/// run the query and write its result as a new image — the CLI face of the
/// derived-table primitive (and how the differential tests it).
fn materialize(args: &[String]) {
    let mut group_target = 65_536u32;
    let mut pos: Vec<&String> = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--row-group-size" {
            group_target = it.next().and_then(|v| v.parse().ok()).unwrap_or_else(|| {
                eprintln!("--row-group-size needs a positive integer");
                exit(2);
            });
        } else {
            pos.push(a);
        }
    }
    let [input, sql, output] = pos[..] else {
        eprintln!("usage: facetful materialize in.facetful \"select …\" out.facetful [--row-group-size N]");
        exit(2);
    };
    let bytes = std::fs::read(input).unwrap_or_else(|e| {
        eprintln!("cannot read {input}: {e}");
        exit(1);
    });
    let mut table = facetful_engine::Table::open(bytes).unwrap_or_else(|e| {
        eprintln!("{input}: {e}");
        exit(1);
    });
    let t0 = std::time::Instant::now();
    let image = facetful_engine::materialize::materialize(&mut table, sql, group_target)
        .unwrap_or_else(|d| {
            eprint!("{}", d.render(sql));
            exit(1);
        });
    let mat_ms = t0.elapsed().as_secs_f64() * 1e3;
    let derived = facetful_engine::Table::open(image.clone()).unwrap_or_else(|e| {
        eprintln!("materialized image failed to open: {e}");
        exit(1);
    });
    std::fs::write(output, &image).unwrap_or_else(|e| {
        eprintln!("cannot write {output}: {e}");
        exit(1);
    });
    eprintln!(
        "{output}: {} rows, {} columns, {} bytes — materialize {mat_ms:.1} ms, {:.1} ms with open + write",
        (0..derived.group_count()).map(|g| derived.group_rows(g)).sum::<usize>(),
        derived.catalog().schema.columns.len(),
        image.len(),
        t0.elapsed().as_secs_f64() * 1e3
    );
}


/// `facetful join left right out --on l=r[,…] [--columns a,b] [--inner]`:
/// the one-shot hash join, written as a new image.
fn join(args: &[String]) {
    let usage = || -> ! {
        eprintln!("usage: facetful join left.facetful right.facetful out.facetful --on l=r[,l2=r2] [--columns a,b] [--inner] [--row-group-size N]");
        exit(2);
    };
    let (mut on, mut columns, mut inner, mut group_target) = (None, Vec::new(), false, 65_536u32);
    let mut pos: Vec<&String> = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--on" => on = it.next().cloned(),
            "--columns" => {
                columns = it.next().map(|c| c.split(',').map(str::to_string).collect()).unwrap_or_default()
            }
            "--inner" => inner = true,
            "--row-group-size" => {
                group_target = it.next().and_then(|v| v.parse().ok()).unwrap_or_else(|| usage())
            }
            _ => pos.push(a),
        }
    }
    let (&[left, right, output], Some(on)) = (pos.as_slice(), on) else { usage() };
    let keys: Vec<(String, String)> = on
        .split(',')
        .map(|p| p.split_once('=').map(|(l, r)| (l.to_string(), r.to_string())).unwrap_or((p.to_string(), p.to_string())))
        .collect();
    let open = |path: &String| {
        let bytes = std::fs::read(path).unwrap_or_else(|e| {
            eprintln!("cannot read {path}: {e}");
            exit(1);
        });
        facetful_engine::Table::open(bytes).unwrap_or_else(|e| {
            eprintln!("{path}: {e}");
            exit(1);
        })
    };
    let (mut l, mut r) = (open(left), open(right));
    let spec = facetful_engine::join::JoinSpec {
        keys,
        left_columns: None,
        columns: if columns.is_empty() { None } else { Some(columns) },
        renames: Vec::new(),
        kind: if inner { facetful_engine::join::JoinKind::Inner } else { facetful_engine::join::JoinKind::Left },
        matched: true,
    };
    let t0 = std::time::Instant::now();
    let image = facetful_engine::join::join(&mut l, &mut r, &spec, group_target).unwrap_or_else(|e| {
        eprintln!("{e}");
        exit(1);
    });
    let join_ms = t0.elapsed().as_secs_f64() * 1e3;
    let out = facetful_engine::Table::open(image.clone()).unwrap_or_else(|e| {
        eprintln!("joined image failed to open: {e}");
        exit(1);
    });
    std::fs::write(output, &image).unwrap_or_else(|e| {
        eprintln!("cannot write {output}: {e}");
        exit(1);
    });
    eprintln!(
        "{output}: {} rows, {} columns, {} bytes — join {join_ms:.1} ms, {:.1} ms with open + write",
        (0..out.group_count()).map(|g| out.group_rows(g)).sum::<usize>(),
        out.catalog().schema.columns.len(),
        image.len(),
        t0.elapsed().as_secs_f64() * 1e3
    );
}
