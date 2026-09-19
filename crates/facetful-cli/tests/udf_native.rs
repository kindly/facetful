//! The CLI's native regexp() (regex crate) agrees with the engine's own LIKE
//! on patterns both can express, and reports pattern errors as query errors.
use std::path::PathBuf;
use std::process::Command;

fn repo(p: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..").join(p)
}

fn run(sql: &str) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_facetful"))
        .arg("query")
        .arg(repo("spikes/facet-spike/data-200000.facetful"))
        .arg(sql)
        .output()
        .expect("run facetful");
    (out.status.success(), String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr))
}

fn count(sql: &str) -> String {
    let (ok, text) = run(sql);
    assert!(ok, "{sql}\n{text}");
    text.lines().nth(2).map(str::trim).unwrap_or("").to_string()
}

#[test]
fn regexp_matches_like_and_reports_bad_patterns() {
    if !repo("spikes/facet-spike/data-200000.facetful").exists() {
        eprintln!("skipping: spike dataset not generated");
        return;
    }
    // a dictionary column (runs over the dictionary) and a text column
    for (re, like) in [
        ("country", "^country_1[0-9]$", "country like 'country_1_'"),
        ("owner", "^owner_2.*7$", "owner like 'owner_2%7'"),
        ("status", "status_[02]", "(status like '%status_0%' or status like '%status_2%')"),
    ]
    .map(|(c, r, l)| (format!("select count(*) from t where regexp({c}, '{r}')"), format!("select count(*) from t where {l}")))
    {
        let (a, b) = (count(&re), count(&like));
        assert_eq!(a, b, "{re}");
        assert_ne!(a, "0");
    }
    // NULL text never matches; a NULL pattern gives NULL (strict)
    assert_eq!(count("select count(*) from t where regexp(country, null)"), "0");
    let (ok, text) = run("select count(*) from t where regexp(country, '(')");
    assert!(!ok || text.contains("regexp"), "{text}");
    assert!(text.contains("user-defined function failed"), "{text}");
}
