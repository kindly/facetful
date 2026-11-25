//! What an image costs to download, and what would make it cost less.
//!
//!   facetful sizes file.facetful   each column's share after compression
//!   facetful order file.facetful   row orders that compress better
//!
//! Size here is what a host sends: the packed image after brotli (or gzip).
//! Both run the `brotli` or `gzip` command, so the numbers are the ones a
//! publisher gets. docs/file-size.sv has the measurements behind this.

use std::io::{Read, Write};
use std::process::{exit, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use facetful_format as fmt;
use fmt::ColumnType;

#[derive(Clone, Copy)]
struct Codec {
    gzip: bool,
    /// brotli quality (gzip is always -9)
    quality: u32,
}

impl Codec {
    fn name(&self) -> String {
        if self.gzip { "gzip -9".into() } else { format!("brotli -q {}", self.quality) }
    }

    /// Bytes of `data` compressed, by the command a publisher would run.
    fn size(&self, data: &[u8]) -> u64 {
        let (cmd, args): (&str, Vec<String>) = if self.gzip {
            ("gzip", vec!["-9".into(), "-c".into()])
        } else {
            ("brotli", vec!["-q".into(), self.quality.to_string(), "-w".into(), "24".into(), "-c".into()])
        };
        let mut child = Command::new(cmd).args(&args).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap_or_else(|e| {
            eprintln!("cannot run the {cmd} command ({e}): it measures the size after compression, install it or pass {}", if self.gzip { "no --gzip" } else { "--gzip" });
            exit(1);
        });
        let mut stdin = child.stdin.take().expect("piped");
        let mut stdout = child.stdout.take().expect("piped");
        let n = std::thread::scope(|s| {
            s.spawn(move || {
                // a compressor that died shows in its exit status below
                let _ = stdin.write_all(data);
            });
            let mut buf = vec![0u8; 1 << 16];
            let mut n = 0u64;
            while let Ok(k) = stdout.read(&mut buf) {
                if k == 0 {
                    break;
                }
                n += k as u64;
            }
            n
        });
        if !child.wait().is_ok_and(|s| s.success()) {
            eprintln!("{cmd} failed");
            exit(1);
        }
        n
    }
}

/// `f(0..n)` on up to `workers` threads, results in order.
fn par<T: Send>(n: usize, workers: usize, f: impl Fn(usize) -> T + Sync) -> Vec<T> {
    let next = AtomicUsize::new(0);
    let out: Mutex<Vec<Option<T>>> = Mutex::new((0..n).map(|_| None).collect());
    std::thread::scope(|s| {
        for _ in 0..workers.clamp(1, n.max(1)) {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= n {
                    break;
                }
                let v = f(i);
                out.lock().unwrap()[i] = Some(v);
            });
        }
    });
    out.into_inner().unwrap().into_iter().map(|v| v.expect("every job ran")).collect()
}

fn mb(n: u64) -> String {
    if n >= 100_000 { format!("{:.2} MB", n as f64 / 1e6) } else { format!("{:.1} KB", n as f64 / 1e3) }
}

fn kind(def: &fmt::ColumnDef) -> String {
    match (def.ty, def.is_dict()) {
        (ColumnType::Utf8, true) => "text, dictionary".into(),
        (ColumnType::Utf8, false) => "text".into(),
        (ty, _) => format!("{ty:?}").to_lowercase(),
    }
}

struct Args {
    path: String,
    codec: Codec,
    jobs: usize,
    depth: usize,
    confirm: bool,
}

fn parse(args: &[String], usage: &str, quality: u32) -> Args {
    let fail = || -> ! {
        eprintln!("usage: {usage}");
        exit(2);
    };
    let mut a = Args { path: String::new(), codec: Codec { gzip: false, quality }, jobs: std::thread::available_parallelism().map_or(4, |n| n.get()), depth: 3, confirm: false };
    let mut it = args.iter();
    let num = |v: Option<&String>| v.and_then(|v| v.parse::<usize>().ok()).filter(|&n| n > 0).unwrap_or_else(|| fail());
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--gzip" => a.codec.gzip = true,
            "--quality" => a.codec.quality = num(it.next()).min(11) as u32,
            "--jobs" => a.jobs = num(it.next()),
            "--depth" => a.depth = num(it.next()),
            "--confirm" => a.confirm = true,
            _ if a.path.is_empty() && !arg.starts_with("--") => a.path = arg.clone(),
            _ => fail(),
        }
    }
    if a.path.is_empty() {
        fail();
    }
    a
}

/// `facetful sizes`: the image's download size as it is and packed, and
/// each column's share of the packed one, largest first.
pub fn sizes(args: &[String]) {
    let a = parse(args, "facetful sizes file.facetful [--gzip] [--quality 0-11] [--jobs N]", 11);
    let image = crate::read_image(&a.path);
    let src: &[u8] = &image;
    let fail = |e: fmt::FormatError| -> ! {
        eprintln!("{}: {e}", a.path);
        exit(1);
    };
    let cat = fmt::read::open(&src).unwrap_or_else(|e| fail(e));
    let packed = fmt::pack::pack(&image).unwrap_or_else(|e| fail(e));
    let shares = fmt::pack::pack_columns(&image, fmt::pack::Opts::ALL).unwrap_or_else(|e| fail(e));
    eprintln!("compressing with {} (the whole image twice, and each column) …", a.codec.name());
    // the two whole images first: they take longest
    let ncols = shares.len();
    let sized = par(ncols + 2, a.jobs, |i| match i {
        0 => a.codec.size(&image),
        1 => a.codec.size(&packed),
        i => a.codec.size(&shares[i - 2]),
    });
    let (as_is, whole) = (sized[0], sized[1]);
    println!("{}: {} rows, {} columns", a.path, cat.total_rows, ncols);
    println!("  as it is   {:>10}   {:>10} after {}", mb(image.len() as u64), mb(as_is), a.codec.name());
    println!("  packed     {:>10}   {:>10} after {}  ({:.1}% less to download)", mb(packed.len() as u64), mb(whole), a.codec.name(), 100.0 - 100.0 * whole as f64 / as_is as f64);
    println!();
    let stored = |ci: usize| -> u64 {
        let groups: u64 = cat.groups.iter().map(|g| g.cols[ci].seg_lens.iter().map(|&l| l as u64).sum::<u64>()).sum();
        groups + cat.dicts[ci].map_or(0, |d| d.offsets_len as u64 + d.bytes_len as u64)
    };
    let mut rows: Vec<usize> = (0..ncols).collect();
    rows.sort_by_key(|&ci| std::cmp::Reverse(sized[ci + 2]));
    let sum: u64 = sized[2..].iter().sum();
    let width = cat.schema.columns.iter().map(|c| c.name.chars().count()).max().unwrap_or(6).clamp(6, 40);
    println!("  {:width$}  {:17}  {:>10}  {:>10}  {:>10}  share", "column", "type", "in image", "packed", "download");
    for ci in rows {
        let def = &cat.schema.columns[ci];
        let name: String = def.name.chars().take(width).collect();
        println!(
            "  {name:width$}  {:17}  {:>10}  {:>10}  {:>10}  {:>4.1}%",
            kind(def),
            mb(stored(ci)),
            mb(shares[ci].len() as u64),
            mb(sized[ci + 2]),
            100.0 * sized[ci + 2] as f64 / sum as f64
        );
    }
    println!();
    println!("  The columns sum to {}; compressed together they come to {}.", mb(sum), mb(whole));
    println!("  A column the page never reads is cheapest left out: facetful materialize, naming the columns to keep.");
}

fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// The image's rows in the order `keys` give (as it is, without keys),
/// packed and compressed: its download size.
fn ordered_size(image: &[u8], keys: &[&str], group_target: u32, codec: Codec) -> Result<u64, String> {
    let mut table = facetful_engine::Table::open(image).map_err(|e| e.to_string())?;
    let mut sql = "select * from t".to_string();
    if !keys.is_empty() {
        sql += &format!(" order by {}", keys.iter().map(|k| quote(k)).collect::<Vec<_>>().join(", "));
    }
    let out = facetful_engine::materialize::materialize(&mut table, &sql, group_target).map_err(|d| d.render(&sql))?;
    drop(table);
    let packed = fmt::pack::pack(&out).map_err(|e| e.to_string())?;
    drop(out);
    Ok(codec.size(&packed))
}

/// `facetful order`: tries sort keys one after another, keeping the one
/// that makes the packed, compressed image smallest, then a second key
/// under it, and so on. A search, not a proof: the best order of all rows is
/// out of reach, and this finds a good one among sorts by a few columns.
pub fn order(args: &[String]) {
    let a = parse(args, "facetful order file.facetful [--gzip] [--quality 0-11] [--depth N] [--jobs N] [--confirm]", 5);
    let image = crate::read_image(&a.path);
    let src: &[u8] = &image;
    let cat = fmt::read::open(&src).unwrap_or_else(|e| {
        eprintln!("{}: {e}", a.path);
        exit(1);
    });
    // keys worth trying: columns whose values repeat or run in order.
    // Floats and free text rarely make a useful first key and are many.
    let candidates: Vec<&str> = cat.schema.columns.iter().filter(|c| c.is_dict() || !matches!(c.ty, ColumnType::Utf8 | ColumnType::Float64)).map(|c| c.name.as_str()).collect();
    // each try holds the image twice over; keep a few at a time
    let jobs = a.jobs.min(4);
    let run = |keys: &[&str], codec: Codec| {
        ordered_size(&image, keys, cat.row_group_target, codec).unwrap_or_else(|e| {
            eprintln!("{}: order by {}: {e}", a.path, keys.join(", "));
            exit(1);
        })
    };
    eprintln!("{}: {} rows; trying {} columns as sort keys, {} deep, with {}", a.path, cat.total_rows, candidates.len(), a.depth, a.codec.name());
    let base = run(&[], a.codec);
    println!("  {:>10}          as it is", mb(base));
    let mut tried: Vec<(u64, Vec<&str>)> = Vec::new();
    let mut keys: Vec<&str> = Vec::new();
    let mut best = base;
    for level in 0..a.depth {
        let next: Vec<&str> = candidates.iter().copied().filter(|c| !keys.contains(c)).collect();
        if next.is_empty() {
            break;
        }
        let sizes = par(next.len(), jobs, |i| {
            let mut k = keys.clone();
            k.push(next[i]);
            run(&k, a.codec)
        });
        let (i, &size) = sizes.iter().enumerate().min_by_key(|&(_, s)| *s).expect("candidates");
        for (c, s) in next.iter().zip(&sizes) {
            let mut k = keys.clone();
            k.push(c);
            tried.push((*s, k));
        }
        // a key has to earn its place: half a percent
        if size as f64 > best as f64 * 0.995 {
            eprintln!("  key {}: nothing gains half a percent, stopping", level + 1);
            break;
        }
        keys.push(next[i]);
        best = size;
        eprintln!("  key {}: {} ({})", level + 1, next[i], mb(size));
    }
    tried.sort();
    for (size, k) in tried.iter().take(12) {
        println!("  {:>10}  {:>+5.1}%  order by {}", mb(*size), 100.0 * *size as f64 / base as f64 - 100.0, k.join(", "));
    }
    println!();
    if keys.is_empty() {
        println!("  No order tried is half a percent smaller than the one the image has. Leave it.");
        return;
    }
    let by = keys.iter().map(|k| quote(k)).collect::<Vec<_>>().join(", ");
    if a.confirm && !a.codec.gzip && a.codec.quality < 11 {
        let full = Codec { gzip: false, quality: 11 };
        let sizes = par(2, 2, |i| if i == 0 { run(&[], full) } else { run(&keys, full) });
        println!("  At brotli -q 11: {} as it is, {} sorted ({:+.1}%).", mb(sizes[0]), mb(sizes[1]), 100.0 * sizes[1] as f64 / sizes[0] as f64 - 100.0);
    }
    println!("  Smallest found: order by {by}");
    println!("  To write it:    facetful materialize {} 'select * from t order by {by}' sorted.facetful", a.path);
    println!("  Row order is also the order rows come back in without ORDER BY, the order a page");
    println!("  sees them while the image loads, and what lets a filter on the sort key skip row");
    println!("  groups. Smaller is not the only thing to choose it by.");
}
