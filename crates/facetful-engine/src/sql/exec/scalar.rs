//! Per-value scalar and temporal functions — the cold path behind computed lanes.

use super::*;

pub(super) const TEMPORAL_FNS: &[&str] =
    &["year", "month", "day", "hour", "minute", "second", "date", "timestamp", "strftime"];

/// The argument whose bound type disambiguates days-vs-ms for a temporal call.
pub(super) fn temporal_arg_index(name: &str) -> usize {
    if name == "strftime" { 1 } else { 0 }
}

use facetful_format::time;

/// Temporal functions need the *bound type* of their argument (Date = days,
/// Timestamp = ms share Val::Int), so they bypass scalar_fn's untyped Vals.
pub(super) fn temporal_fn(name: &str, aty: Ty, args: &[Val]) -> Val {
    let x = &args[temporal_arg_index(name)];
    if x.is_null() {
        return Val::Null;
    }
    // the temporal argument as (days, ms) where applicable
    let ms_of = |v: &Val| match (aty, v) {
        (Ty::Date, Val::Int(d)) => Some(d * time::MS_PER_DAY),
        (_, Val::Int(ms)) => Some(*ms),
        _ => None,
    };
    let days_of = |v: &Val| match (aty, v) {
        (Ty::Timestamp, Val::Int(ms)) => Some(ms.div_euclid(time::MS_PER_DAY)),
        (_, Val::Int(d)) => Some(*d),
        _ => None,
    };
    match name {
        "year" | "month" | "day" => {
            let Some(days) = days_of(x) else { return Val::Null };
            let (y, m, d) = time::civil_from_days(days);
            Val::Int(match name {
                "year" => y,
                "month" => m as i64,
                _ => d as i64,
            })
        }
        "hour" | "minute" | "second" => {
            let Some(ms) = ms_of(x) else { return Val::Null };
            let t = ms.rem_euclid(time::MS_PER_DAY) / 1000;
            Val::Int(match name {
                "hour" => t / 3600,
                "minute" => t / 60 % 60,
                _ => t % 60,
            })
        }
        "date" => match x {
            Val::Text(s) => time::parse_date(s).map(Val::Int).unwrap_or(Val::Null),
            _ => days_of(x).map(Val::Int).unwrap_or(Val::Null),
        },
        "timestamp" => match x {
            Val::Text(s) => time::parse_timestamp(s).map(Val::Int).unwrap_or(Val::Null),
            _ => ms_of(x).map(Val::Int).unwrap_or(Val::Null),
        },
        "strftime" => {
            let Val::Text(fmt) = &args[0] else { return Val::Null };
            let Some(ms) = ms_of(x) else { return Val::Null };
            let days = ms.div_euclid(time::MS_PER_DAY);
            let (y, m, d) = time::civil_from_days(days);
            let t = ms.rem_euclid(time::MS_PER_DAY) / 1000;
            let mut out = String::new();
            let mut it = fmt.chars();
            while let Some(c) = it.next() {
                if c != '%' {
                    out.push(c);
                    continue;
                }
                match it.next() {
                    Some('Y') => out.push_str(&format!("{y:04}")),
                    Some('m') => out.push_str(&format!("{m:02}")),
                    Some('d') => out.push_str(&format!("{d:02}")),
                    Some('H') => out.push_str(&format!("{:02}", t / 3600)),
                    Some('M') => out.push_str(&format!("{:02}", t / 60 % 60)),
                    Some('S') => out.push_str(&format!("{:02}", t % 60)),
                    Some('s') => out.push_str(&(ms.div_euclid(1000)).to_string()),
                    Some('%') => out.push('%'),
                    Some(other) => {
                        out.push('%');
                        out.push(other);
                    }
                    None => out.push('%'),
                }
            }
            Val::text(out)
        }
        _ => unreachable!("not a temporal function: {name}"),
    }
}

/// scalar function on materialized values — cold path + grouped-context eval
pub(super) fn scalar_fn(name: &str, mut args: Vec<Val>) -> Val {
    match name {
        "isnull" => Val::Bool(args[0].is_null()),
        "coalesce" | "ifnull" => args.into_iter().find(|v| !v.is_null()).unwrap_or(Val::Null),
        "nullif" => {
            if args[0] == args[1] { Val::Null } else { args.swap_remove(0) }
        }
        "trim" | "ltrim" | "rtrim" => match (&args[0], args.get(1)) {
            (Val::Text(s), sel) => {
                // SQLite semantics: default trims spaces only; 2-arg form trims
                // any character present in the second argument
                let set: Vec<char> = match sel {
                    None => vec![' '],
                    Some(Val::Text(c)) => c.chars().collect(),
                    _ => return Val::Null,
                };
                let f = |c: char| set.contains(&c);
                Val::text(match name {
                    "trim" => s.trim_matches(f),
                    "ltrim" => s.trim_start_matches(f),
                    _ => s.trim_end_matches(f),
                })
            }
            _ => Val::Null,
        },
        "replace" => match (&args[0], &args[1], &args[2]) {
            (Val::Text(s), Val::Text(from), Val::Text(to)) => {
                // empty needle: SQLite returns the input unchanged
                if from.is_empty() { Val::Text(s.clone()) } else { Val::text(s.replace(&**from, to)) }
            }
            _ => Val::Null,
        },
        "instr" => match (&args[0], &args[1]) {
            // 1-based character position of the first occurrence, 0 = absent
            (Val::Text(s), Val::Text(sub)) => match s.find(&**sub) {
                Some(byte) => Val::Int(s[..byte].chars().count() as i64 + 1),
                None => Val::Int(0),
            },
            _ => Val::Null,
        },
        "sign" => match args[0].as_f64() {
            Some(f) if f > 0.0 => Val::Int(1),
            Some(f) if f < 0.0 => Val::Int(-1),
            Some(_) => Val::Int(0),
            None => Val::Null,
        },
        "sqrt" | "exp" | "ln" | "pow" | "power" => {
            let (Some(a), b) = (args[0].as_f64(), args.get(1).and_then(|v| v.as_f64())) else {
                return Val::Null;
            };
            let r = match name {
                "sqrt" => a.sqrt(),
                "exp" => a.exp(),
                "ln" => {
                    if a <= 0.0 { return Val::Null } else { a.ln() }
                }
                _ => match b {
                    Some(b) => a.powf(b),
                    None => return Val::Null,
                },
            };
            // out-of-domain (sqrt(-1), pow(-1, .5)) is NULL, matching SQLite
            if r.is_nan() { Val::Null } else { Val::Float(r) }
        }
        "in" => {
            let needle = args.remove(0);
            if needle.is_null() {
                return Val::Null;
            }
            let mut saw_null = false;
            for v in &args {
                match needle.eq_sql(v) {
                    Some(true) => return Val::Bool(true),
                    None => saw_null = true,
                    _ => {}
                }
            }
            if saw_null { Val::Null } else { Val::Bool(false) }
        }
        "between" => {
            let (x, lo, hi) = (args[0].clone(), args[1].clone(), args[2].clone());
            if x.is_null() || lo.is_null() || hi.is_null() {
                return Val::Null;
            }
            Val::Bool(
                x.cmp_sql(&lo) != core::cmp::Ordering::Less
                    && x.cmp_sql(&hi) != core::cmp::Ordering::Greater,
            )
        }
        "like" => match (&args[0], &args[1]) {
            (Val::Text(s), Val::Text(p)) => Val::Bool(like_match(p, s)),
            _ => Val::Null,
        },
        "if" | "case" => {
            let mut i = 0;
            while i + 1 < args.len() {
                if matches!(args[i], Val::Bool(true)) {
                    return args[i + 1].clone();
                }
                i += 2;
            }
            if args.len() % 2 == 1 { args.last().unwrap().clone() } else { Val::Null }
        }
        "concat" => {
            let mut out = String::new();
            for v in &args {
                match v {
                    Val::Null => return Val::Null,
                    Val::Text(s) => out.push_str(s),
                    Val::Int(i) => out.push_str(&i.to_string()),
                    Val::Float(f) => out.push_str(&f.to_string()),
                    Val::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
                }
            }
            Val::text(out)
        }
        "lower" | "upper" => match &args[0] {
            Val::Text(s) => {
                Val::text(if name == "lower" { s.to_lowercase() } else { s.to_uppercase() })
            }
            _ => Val::Null,
        },
        "length" => match &args[0] {
            Val::Text(s) => Val::Int(s.chars().count() as i64),
            _ => Val::Null,
        },
        "substr" => match &args[0] {
            Val::Text(s) => {
                let start = match args.get(1).and_then(Val::as_f64) {
                    Some(f) => (f as i64 - 1).max(0) as usize,
                    None => return Val::Null,
                };
                let len = args.get(2).and_then(Val::as_f64).map(|f| f as usize);
                let chars: Vec<char> = s.chars().collect();
                let end = len.map(|l| (start + l).min(chars.len())).unwrap_or(chars.len());
                if start >= chars.len() {
                    Val::text("")
                } else {
                    Val::text(chars[start..end].iter().collect::<String>())
                }
            }
            _ => Val::Null,
        },
        "abs" => match &args[0] {
            Val::Int(i) => Val::Int(i.abs()),
            Val::Float(f) => Val::Float(f.abs()),
            _ => Val::Null,
        },
        "floor" | "ceil" | "round" => match args[0].as_f64() {
            Some(f) => {
                let r = match name {
                    "floor" => f.floor(),
                    "ceil" => f.ceil(),
                    _ => {
                        let digits = args.get(1).and_then(Val::as_f64).unwrap_or(0.0) as i32;
                        let m = 10f64.powi(digits);
                        (f * m).round() / m
                    }
                };
                if matches!(args[0], Val::Int(_)) && name != "round" {
                    Val::Int(r as i64)
                } else {
                    Val::Float(r)
                }
            }
            None => Val::Null,
        },
        "int" => match &args[0] {
            Val::Int(i) => Val::Int(*i),
            Val::Float(f) => Val::Int(*f as i64),
            Val::Text(s) => s.trim().parse::<i64>().map(Val::Int).unwrap_or(Val::Null),
            Val::Bool(b) => Val::Int(*b as i64),
            Val::Null => Val::Null,
        },
        "float" => match &args[0] {
            Val::Int(i) => Val::Float(*i as f64),
            Val::Float(f) => Val::Float(*f),
            Val::Text(s) => s.trim().parse::<f64>().map(Val::Float).unwrap_or(Val::Null),
            Val::Bool(b) => Val::Float(*b as i64 as f64),
            Val::Null => Val::Null,
        },
        "text" => match &args[0] {
            Val::Null => Val::Null,
            Val::Text(s) => Val::Text(s.clone()),
            Val::Int(i) => Val::text(i.to_string()),
            Val::Float(f) => Val::text(f.to_string()),
            Val::Bool(b) => Val::text(if *b { "true" } else { "false" }),
        },
        other => unreachable!("unbound scalar function {other}"),
    }
}
