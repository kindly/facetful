//! How much smaller can an image be on the wire? Each column of an image is
//! re-encoded in several candidate wire forms, and each form is compressed
//! with brotli q11 (window 24) and gzip -9, the two ways an image is served.
//! The stored form, compressed the same way, is the baseline: an encoding
//! only counts for what it saves after compression.
//!
//!   cargo run --release -- cols <image> [column-filter]   per column, per encoding
//!   cargo run --release -- whole <image>                   the image as served
//!   cargo run --release -- pack <image> [variant,…]         packed whole (pack.rs): each idea
//!                                                          alone and all together, round trip checked
//!     KEEP=<dir> keeps the packed image with everything on; NOSIZE=1 skips compression
//!
//! Results are cached by content in $TMPDIR/size-bench-cache.tsv, so adding an
//! encoding only compresses what is new. Docs: docs/file-size.sv.

mod enc;
mod img;
mod pack;

use std::collections::HashMap;
use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use img::Img;

pub struct Job {
    pub column: String,
    pub kind: String,
    pub variant: &'static str,
    pub make: Box<dyn Fn(&Img) -> Vec<u8> + Send + Sync>,
}

#[derive(Clone, Copy)]
pub struct Sizes {
    pub raw: u64,
    pub br: u64,
    pub gz: u64,
}

fn pipe(cmd: &str, args: &[&str], data: &[u8]) -> u64 {
    let mut child = Command::new(cmd).args(args).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().expect("spawn");
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let n = std::thread::scope(|s| {
        s.spawn(move || {
            stdin.write_all(data).expect("write");
        });
        let mut buf = vec![0u8; 1 << 16];
        let mut n = 0u64;
        loop {
            let k = stdout.read(&mut buf).expect("read");
            if k == 0 {
                break;
            }
            n += k as u64;
        }
        n
    });
    assert!(child.wait().expect("wait").success());
    n
}

fn fnv(data: &[u8]) -> u64 {
    // 8 bytes at a time: a cache key, not a checksum
    let mut h = 0xcbf29ce484222325u64 ^ data.len() as u64;
    let mut chunks = data.chunks_exact(8);
    for c in &mut chunks {
        h = (h ^ u64::from_le_bytes(c.try_into().unwrap())).wrapping_mul(0x100000001b3).rotate_left(23);
    }
    for &b in chunks.remainder() {
        h = (h ^ b as u64).wrapping_mul(0x100000001b3);
    }
    h
}

pub struct Cache {
    path: String,
    map: Mutex<HashMap<(u64, u64), (u64, u64)>>,
}

impl Cache {
    pub fn open() -> Cache {
        let path = format!("{}/size-bench-cache.tsv", std::env::var("TMPDIR").unwrap_or("/tmp".into()));
        let mut map = HashMap::new();
        if let Ok(s) = std::fs::read_to_string(&path) {
            for l in s.lines() {
                let f: Vec<u64> = l.split('\t').filter_map(|x| x.parse().ok()).collect();
                if f.len() == 4 {
                    map.insert((f[0], f[1]), (f[2], f[3]));
                }
            }
        }
        Cache { path, map: Mutex::new(map) }
    }

    pub fn sizes(&self, data: &[u8]) -> Sizes {
        let key = (fnv(data), data.len() as u64);
        if let Some(&(br, gz)) = self.map.lock().unwrap().get(&key) {
            return Sizes { raw: key.1, br, gz };
        }
        let br = pipe("brotli", &["-q", "11", "-w", "24", "-c"], data);
        let gz = pipe("gzip", &["-9", "-c"], data);
        let mut m = self.map.lock().unwrap();
        m.insert(key, (br, gz));
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&self.path).unwrap();
        writeln!(f, "{}\t{}\t{}\t{}", key.0, key.1, br, gz).unwrap();
        Sizes { raw: key.1, br, gz }
    }
}

pub fn run(img: &Img, jobs: &[Job], cache: &Cache) -> Vec<Sizes> {
    let next = AtomicUsize::new(0);
    let out = Mutex::new(vec![None; jobs.len()]);
    let workers: usize = std::env::var("JOBS").ok().and_then(|s| s.parse().ok()).unwrap_or(12);
    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= jobs.len() {
                    break;
                }
                let data = (jobs[i].make)(img);
                let sz = cache.sizes(&data);
                drop(data);
                eprintln!("  done {} / {}", jobs[i].column, jobs[i].variant);
                out.lock().unwrap()[i] = Some(sz);
            });
        }
    });
    out.into_inner().unwrap().into_iter().map(|s| s.unwrap()).collect()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(|s| s.as_str()).unwrap_or("");
    let Some(path) = args.get(2) else {
        eprintln!("usage: size-bench cols|whole|pack <image> [column-filter | variants]");
        std::process::exit(2);
    };
    let img = Img::open(path);
    let cache = Cache::open();
    match cmd {
        "whole" => {
            let s = cache.sizes(&img.bytes);
            println!("whole\t{}\t{}\t{}", s.raw, s.br, s.gz);
        }
        "cols" => {
            let filter = args.get(3).cloned().unwrap_or_default();
            let jobs: Vec<Job> = enc::jobs(&img).into_iter().filter(|j| j.column.contains(&filter)).collect();
            let sizes = run(&img, &jobs, &cache);
            println!("column\tkind\tvariant\traw\tbr\tgz");
            for (j, s) in jobs.iter().zip(&sizes) {
                println!("{}\t{}\t{}\t{}\t{}\t{}", j.column, j.kind, j.variant, s.raw, s.br, s.gz);
            }
        }
        "pack" => {
            use pack::Opts;
            let n = Opts::NONE;
            let sets: Vec<(&str, Opts)> = vec![
                ("packed, nothing re-encoded", n),
                ("text-sep", Opts { text_sep: true, ..n }),
                ("text-sep+numsuffix", Opts { text_sep: true, numsuffix: true, ..n }),
                ("dict-sep", Opts { dict_sep: true, ..n }),
                ("alp", Opts { alp: true, ..n }),
                ("delta", Opts { delta: true, ..n }),
                ("no-words", Opts { words: None, ..Opts::ALL }),
                ("words64", Opts { words: Some(64), ..n }),
                ("all", Opts::ALL),
                ("all-words32", Opts { words: Some(32), ..Opts::ALL }),
            ];
            let only = args.get(3).cloned().unwrap_or_default();
            let sets: Vec<_> = sets.into_iter().filter(|s| only.is_empty() || only.split(',').any(|o| s.0 == o)).collect();
            println!("variant\traw\tbr\tgz\tunpack_ms\tunpack_MB_s");
            let lines = Mutex::new(Vec::new());
            std::thread::scope(|s| {
                for (i, (name, o)) in sets.iter().enumerate() {
                    let (img, cache, lines) = (&img, &cache, &lines);
                    s.spawn(move || {
                        let wire = pack::pack(&img.bytes, &img.cat, *o);
                        let mut best = f64::MAX;
                        for _ in 0..3 {
                            let t = std::time::Instant::now();
                            let back = pack::unpack(&wire).expect("unpack");
                            best = best.min(t.elapsed().as_secs_f64());
                            assert!(back == img.bytes, "{name}: the unpacked image differs");
                        }
                        if let (Ok(dir), true) = (std::env::var("KEEP"), *name == "all" || std::env::var("KEEP_ALL").is_ok()) {
                            std::fs::write(format!("{dir}/{}.fctz", name.replace(' ', "_").replace(',', "")), &wire).unwrap();
                        }
                        let sz = if std::env::var("NOSIZE").is_ok() { Sizes { raw: wire.len() as u64, br: 0, gz: 0 } } else { cache.sizes(&wire) };
                        lines.lock().unwrap().push((i, format!("{name}\t{}\t{}\t{}\t{:.0}\t{:.0}", sz.raw, sz.br, sz.gz, best * 1e3, img.bytes.len() as f64 / 1e6 / best)));
                    });
                }
            });
            let mut lines = lines.into_inner().unwrap();
            lines.sort();
            lines.iter().for_each(|l| println!("{}", l.1));
        }
        "tuple" => {
            // tuple <image> "colA|colB|…": the columns apart, and row by row as one
            let names: Vec<&str> = args[3].split('|').collect();
            let cols: Vec<usize> = names.iter().map(|n| (0..img.ncols()).find(|&c| img.def(c).name == *n).expect("column")).collect();
            for alp in [false, true] {
                let apart = cache.sizes(&enc::tuple(&img, &cols, false, alp));
                let together = cache.sizes(&enc::tuple(&img, &cols, true, alp));
                println!("{}\talp={alp}\tapart br {} gz {}\trow by row br {} gz {}", args[3], apart.br, apart.gz, together.br, together.gz);
            }
        }
        _ => {
            eprintln!("unknown command {cmd}");
            std::process::exit(2);
        }
    }
}
