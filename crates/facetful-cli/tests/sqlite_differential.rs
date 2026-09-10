//! Differential correctness: the same data and the same SQL run through both
//! facetful and SQLite; results must agree cell-for-cell (floats to 1e-9 rel).
//! Queries use only the dialect intersection — the LLM idiom set is exactly
//! the SQL both engines accept, which is the point of having chosen it.
//!
//! Skips (with a note) when sqlite3 or the spike dataset isn't available.

use facetful_engine::sql::exec::Val;
use facetful_engine::sql::run_query;
use facetful_engine::Table;
use std::path::PathBuf;
use std::process::Command;

fn repo(p: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..").join(p)
}

const QUERIES: &[&str] = &[
    // counts, distinct, null-aware counting
    "select count(*), count(capacity), count(distinct country) from t",
    // group + aggregates + order + limit
    "select country, count(*) as n, sum(capacity) as total from t \
     group by country order by n desc, country limit 10",
    // filter idioms: IN + string BETWEEN
    "select fuel, avg(capacity) as a, min(capacity) as lo, max(capacity) as hi from t \
     where status in ('status_0', 'status_1') and year between 'year_10' and 'year_19' \
     group by fuel order by fuel",
    // IS NULL and IS NOT NULL
    "select id from t where capacity is null order by id limit 20",
    "select count(*) from t where capacity is not null",
    // string ops: || upper lower substr length
    "select upper(country) || '-' || lower(status) as tag, substr(owner, 1, 8), length(fuel) \
     from t where id < 50 order by id",
    // CASE WHEN with the expression repeated in GROUP BY
    "select case when capacity > 100 then 'big' when capacity > 10 then 'mid' else 'small' end as k, \
     count(*) from t \
     group by case when capacity > 100 then 'big' when capacity > 10 then 'mid' else 'small' end \
     order by k",
    // CAST + grouping on it
    "select cast(capacity as integer) as c, count(*) from t \
     where capacity is not null and capacity < 5 \
     group by cast(capacity as integer) order by c",
    // LIKE / NOT LIKE
    "select count(*) from t where country like 'country_1%' and owner not like '%7'",
    // arithmetic incl. truncating int division and modulo
    "select id, id * 3 + 1, id / 7, id % 7, capacity * 2 from t \
     where id between 100 and 120 order by id",
    // coalesce + round
    "select round(sum(coalesce(capacity, 0)), 1) from t where id < 1000",
    // three-valued logic: NOT over a null comparison
    "select count(*) from t where not (capacity > 50)",
    // NULL ordering (both put NULL first ascending) with tiebreak
    "select capacity, id from t order by capacity, id limit 25",
    // empty aggregate
    "select count(*), sum(capacity), avg(capacity) from t where id < 0",
    // expression over aggregates
    "select sum(capacity) / count(capacity) as manual_avg, count(*) from t \
     where country = 'country_3'",
    // text scalar batch: trim family (1- and 2-arg), replace, instr
    "select trim('  ' || status || '  ') as a, ltrim(country, 'country_') as b, \
     rtrim(owner, '0123456789') as c, replace(fuel, 'fuel', 'F') as d, \
     instr(owner, '_') as e from t where id between 40 and 60 order by id",
    // nullif / ifnull over real NULLs
    "select count(nullif(status, 'status_1')), round(sum(ifnull(capacity, 0)), 1), \
     count(*) from t where id < 2000",
    // math scalar batch (sqlite needs -DSQLITE_ENABLE_MATH_FUNCTIONS; distro CLIs have it)
    "select id, sign(capacity - 100), round(sqrt(capacity), 4), round(pow(capacity, 0.5), 4), \
     round(ln(capacity + 1), 4), round(exp(1.0), 4) from t \
     where capacity is not null and id between 500 and 520 order by id",
    // select * expansion (schema order) incl. star alongside expressions
    "select * from t where id < 3 order by id",
    "select *, id * 2 from t where id between 10 and 12 order by id",
    // group_concat: default and explicit separator, scan order matches
    "select country, group_concat(status) as gs, group_concat(fuel, '|') as gf from t \
     where id < 300 group by country order by country",
];

#[test]
fn facetful_matches_sqlite() {
    let csv = repo("spikes/facet-spike/data-200000.csv");
    let facetful_file = repo("spikes/facet-spike/data-200000.facetful");
    if !csv.exists() || !facetful_file.exists() {
        eprintln!("skipping: spike dataset not generated");
        return;
    }
    if Command::new("sqlite3").arg("--version").output().is_err() {
        eprintln!("skipping: sqlite3 not on PATH");
        return;
    }

    // Build the SQLite db with matching types + NULL semantics ('' -> NULL).
    let db = std::env::temp_dir().join(format!("facetful-diff-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&db);
    let setup = format!(
        "create table t (country TEXT, status TEXT, fuel TEXT, region TEXT, owner TEXT, \
         year TEXT, capacity REAL, id INTEGER);\n\
         .mode csv\n.import --skip 1 '{}' t\n\
         update t set capacity = NULL where capacity = '';",
        csv.display()
    );
    let mut child = Command::new("sqlite3")
        .arg(&db)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn sqlite3");
    {
        use std::io::Write;
        child.stdin.as_mut().unwrap().write_all(setup.as_bytes()).unwrap();
    }
    let out = child.wait_with_output().expect("run sqlite3 setup");
    assert!(out.status.success(), "sqlite setup failed: {}", String::from_utf8_lossy(&out.stderr));

    let bytes = std::fs::read(&facetful_file).unwrap();
    let mut table = Table::open(bytes).unwrap();

    for sql in QUERIES {
        // facetful
        let mut ours = run_query(&mut table, sql).unwrap_or_else(|d| panic!("{}", d.render(sql)));
        ours.ensure_rows();
        let ours: Vec<Vec<String>> = ours
            .rows
            .iter()
            .map(|row| row.iter().map(render_val).collect())
            .collect();

        // sqlite
        let out = Command::new("sqlite3")
            .arg("-csv")
            .arg(&db)
            .arg(sql)
            .output()
            .expect("run sqlite3");
        assert!(
            out.status.success(),
            "sqlite failed on: {sql}\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let theirs = parse_csv(&String::from_utf8_lossy(&out.stdout));

        assert_eq!(
            ours.len(),
            theirs.len(),
            "row count mismatch on: {sql}\nfacetful={ours:?}\nsqlite={theirs:?}"
        );
        for (r, (a, b)) in ours.iter().zip(&theirs).enumerate() {
            assert_eq!(a.len(), b.len(), "column count mismatch on: {sql} row {r}");
            for (c, (x, y)) in a.iter().zip(b).enumerate() {
                if !cells_equal(x, y) {
                    panic!(
                        "cell mismatch on: {sql}\nrow {r} col {c}: facetful='{x}' sqlite='{y}'"
                    );
                }
            }
        }
    }
    let _ = std::fs::remove_file(&db);
    eprintln!("differential: {} queries agree with SQLite", QUERIES.len());
}

fn render_val(v: &Val) -> String {
    match v {
        Val::Null => "".into(),
        Val::Bool(b) => if *b { "1".into() } else { "0".into() },
        Val::Int(i) => i.to_string(),
        Val::Float(f) => f.to_string(),
        Val::Text(s) => s.to_string(),
    }
}

fn cells_equal(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    match (a.parse::<f64>(), b.parse::<f64>()) {
        (Ok(x), Ok(y)) => (x - y).abs() <= x.abs().max(y.abs()) * 1e-9 + 1e-12,
        _ => false,
    }
}

/// Minimal CSV parse for sqlite3 -csv output (quotes only around strings that need them).
fn parse_csv(text: &str) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    for line in text.lines() {
        let mut row = Vec::new();
        let mut field = String::new();
        let mut in_q = false;
        let mut chars = line.chars().peekable();
        while let Some(ch) = chars.next() {
            if in_q {
                if ch == '"' {
                    if chars.peek() == Some(&'"') {
                        chars.next();
                        field.push('"');
                    } else {
                        in_q = false;
                    }
                } else {
                    field.push(ch);
                }
            } else {
                match ch {
                    '"' => in_q = true,
                    ',' => row.push(std::mem::take(&mut field)),
                    '\r' => {}
                    _ => field.push(ch),
                }
            }
        }
        row.push(field);
        rows.push(row);
    }
    rows
}
