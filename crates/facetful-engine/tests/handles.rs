//! Retained results (docs/result-handles.sv): any block of a prepared
//! statement's rows equals the same rows of the statement run whole, before
//! and after `sort`, for rows of the table (gathered a page at a time) and
//! for kept results (grouped, joined). A handle's ties are in table order,
//! which the reference gets by ordering on the unique `id` last.

use facetful_engine::format::write::{ColumnChunk, DictData, SegmentData, Writer};
use facetful_engine::format::{flags, ColumnDef, ColumnType, Schema};
use facetful_engine::sql::exec::{OutCol, QueryResult};
use facetful_engine::sql::{prepare, run_query, Prepared};
use facetful_engine::Table;

const ROWS: usize = 700;
const GROUP: usize = 64;
const TITLES: [&str; 9] = [
    "Mental health support",
    "Youth sport in London",
    "MENTAL HEALTH and wellbeing",
    "Climate change adaptation",
    "São Paulo youth project",
    "Community sport fund",
    "münchen health centre",
    "",
    "London climate youth",
];
const ORGS: [&str; 5] = ["Sport England", "Mind", "Climate Trust", "Youth London", "Health Board"];

/// 700 rows in groups of 64: title plain text (NULL every 13th row), org a
/// dictionary column (NULL every 17th), amount float with many ties (NULL
/// every 7th), year int16, id int32 = the row number.
fn table() -> Table<Vec<u8>> {
    let schema = Schema {
        columns: vec![
            ColumnDef { name: "title".into(), ty: ColumnType::Utf8, flags: 0 },
            ColumnDef { name: "org".into(), ty: ColumnType::Utf8, flags: flags::DICTIONARY | flags::CODES_U8 },
            ColumnDef { name: "amount".into(), ty: ColumnType::Float64, flags: 0 },
            ColumnDef { name: "year".into(), ty: ColumnType::Int16, flags: 0 },
            ColumnDef { name: "id".into(), ty: ColumnType::Int32, flags: 0 },
        ],
    };
    let mut doffs = vec![0u32];
    let mut dbytes = Vec::new();
    for s in ORGS {
        dbytes.extend_from_slice(s.as_bytes());
        doffs.push(dbytes.len() as u32);
    }
    let dicts = [None, Some(DictData { offsets: doffs, bytes: dbytes }), None, None, None];
    let mut w = Writer::new(schema, vec![], GROUP as u32, &dicts);
    for start in (0..ROWS).step_by(GROUP) {
        let end = (start + GROUP).min(ROWS);
        let n = end - start;
        let validity = |null: &dyn Fn(usize) -> bool| {
            let mut v = vec![0u8; n.div_ceil(8)];
            let mut nulls = 0u32;
            for i in start..end {
                if null(i) {
                    nulls += 1;
                } else {
                    v[(i - start) / 8] |= 1 << ((i - start) % 8);
                }
            }
            (v, nulls)
        };
        let mut toffs = vec![0u32];
        let mut tbytes = Vec::new();
        for i in start..end {
            if i % 13 != 0 {
                tbytes.extend_from_slice(format!("{} {}", TITLES[i % TITLES.len()], i % 31).as_bytes());
            }
            toffs.push(tbytes.len() as u32);
        }
        let (tvalid, tnull) = validity(&|i| i % 13 == 0);
        let codes: Vec<u8> = (start..end).map(|i| ((i * 7) % ORGS.len()) as u8).collect();
        let (ovalid, onull) = validity(&|i| i % 17 == 0);
        let amounts: Vec<u8> = (start..end).flat_map(|i| (((i * 37) % 23) as f64 * 1.5 - 4.0).to_le_bytes()).collect();
        let (avalid, anull) = validity(&|i| i % 7 == 0);
        let years: Vec<u8> = (start..end).flat_map(|i| (2000 + (i % 5) as i16).to_le_bytes()).collect();
        let ids: Vec<u8> = (start..end).flat_map(|i| (i as i32).to_le_bytes()).collect();
        w.write_group(n as u32, &[
            ColumnChunk { data: SegmentData::Utf8 { offsets: &toffs, bytes: &tbytes }, validity: Some(&tvalid), null_count: tnull },
            ColumnChunk { data: SegmentData::Codes8(&codes), validity: Some(&ovalid), null_count: onull },
            ColumnChunk { data: SegmentData::Fixed(&amounts), validity: Some(&avalid), null_count: anull },
            ColumnChunk { data: SegmentData::Fixed(&years), validity: None, null_count: 0 },
            ColumnChunk { data: SegmentData::Fixed(&ids), validity: None, null_count: 0 },
        ]);
    }
    Table::open(w.finish()).unwrap()
}

/// A result's rows, each as text.
fn rows_of(mut r: QueryResult) -> Vec<String> {
    r.ensure_rows();
    r.rows.iter().map(|row| format!("{row:?}")).collect()
}

fn query(t: &mut Table<Vec<u8>>, sql: &str) -> Vec<String> {
    rows_of(run_query(t, sql).unwrap_or_else(|d| panic!("{}", d.render(sql))))
}

/// Blocks of the handle against the same rows of `want` (the statement's
/// whole result): ends, a block across row groups, one past the end, all.
fn check_pages(t: &mut Table<Vec<u8>>, h: &Prepared, want: &[String], what: &str) {
    assert_eq!(h.rows(), want.len(), "{what}: row count");
    let n = want.len();
    for (start, end) in [(0, 10), (5, 77), (n / 2, n / 2 + 130), (n.saturating_sub(3), n + 50), (n + 5, n + 9), (0, n), (4, 4)] {
        let got = rows_of(h.page(t, start, end, None).unwrap_or_else(|e| panic!("{what}: {e}")));
        let (s, e) = (start.min(n), end.clamp(start.min(n), n));
        assert_eq!(got, want[s..e], "{what}: rows {start}..{end}");
    }
}

const SELECTS: [&str; 5] = [
    "*",
    "id, title, org",
    "id, upper(title) as shout, amount * 2 as twice, year + 1 as next, org is null as no_org",
    "org, coalesce(title, 'none') as t, id",
    "amount, id",
];
const WHERES: [&str; 6] = [
    "",
    "where amount > 10",
    "where org = 'Mind' or title is null",
    "where year in (2001, 2003) and title like '%health%'",
    "where amount is null",
    "where id < 0",
];
const ORDERS: [&str; 7] = ["", "amount", "amount desc", "org, amount desc", "title desc", "year desc, org", "length(title), amount"];

/// `order` made total: a handle's ties are in table order, and `id` is the row number.
fn total(order: &str) -> String {
    if order.is_empty() { "id".into() } else { format!("{order}, id") }
}

#[test]
fn pages_equal_the_query() {
    let mut t = table();
    for sel in SELECTS {
        for wh in WHERES {
            for order in ORDERS {
                for window in ["", "limit 40", "limit 150 offset 30", "limit 100000 offset 695"] {
                    let by = if order.is_empty() { String::new() } else { format!("order by {order}") };
                    let sql = format!("select {sel} from t {wh} {by} {window}");
                    let h = prepare(&mut t, &sql).unwrap_or_else(|d| panic!("{}", d.render(&sql)));
                    assert!(h.is_rows(), "{sql}");
                    assert_eq!(h.has_row_ids(), !(wh.is_empty() && order.is_empty()), "{sql}");
                    let want = query(&mut t, &format!("select {sel} from t {wh} order by {} {window}", total(order)));
                    check_pages(&mut t, &h, &want, &sql);
                }
            }
        }
    }
}

#[test]
fn sort_equals_the_query_with_that_order() {
    let mut t = table();
    for sel in SELECTS {
        for wh in WHERES {
            for window in ["", "limit 150 offset 30"] {
                let sql = format!("select {sel} from t {wh} order by year {window}");
                let mut h = prepare(&mut t, &sql).unwrap();
                // every order after every other: a sort does not depend on the one before
                for order in ORDERS.iter().chain(ORDERS.iter().rev()) {
                    h.sort(&mut t, order, &mut facetful_engine::sql::NoCatalog).unwrap_or_else(|e| panic!("{sql} / {order}: {e}"));
                    let want = query(&mut t, &format!("select {sel} from t {wh} order by {} {window}", total(order)));
                    check_pages(&mut t, &h, &want, &format!("{sql} sorted by '{order}'"));
                }
            }
        }
    }
}

#[test]
fn sort_by_alias_and_position_and_bad_orders() {
    let mut t = table();
    let sql = "select id, amount * 2 as twice, title from t where year < 2003";
    let mut h = prepare(&mut t, sql).unwrap();
    let cat = &mut facetful_engine::sql::NoCatalog;
    h.sort(&mut t, "twice desc", cat).unwrap();
    let want = query(&mut t, "select id, amount * 2 as twice, title from t where year < 2003 order by twice desc, id");
    check_pages(&mut t, &h, &want, "alias");
    h.sort(&mut t, "3", cat).unwrap();
    let by_title = query(&mut t, "select id, amount * 2 as twice, title from t where year < 2003 order by 3, id");
    check_pages(&mut t, &h, &by_title, "position");
    // the table's own name or alias qualifies a key, as in the statement
    let mut q = prepare(&mut t, "select x.id, x.title from t x where x.year < 2003").unwrap();
    q.sort(&mut t, "x.title desc", cat).unwrap();
    let want = query(&mut t, "select x.id, x.title from t x where x.year < 2003 order by x.title desc, x.id");
    check_pages(&mut t, &q, &want, "qualified key");
    // a failed sort leaves the result as it was
    for bad in ["nope", "id limit 3", "count(*)", "id; select 1", "id asc desc"] {
        let e = h.sort(&mut t, bad, cat).err().unwrap_or_else(|| panic!("sort by '{bad}' was accepted"));
        assert!(e.starts_with("error: "), "{bad}: {e}");
        check_pages(&mut t, &h, &by_title, &format!("after the refused sort '{bad}'"));
    }
}

#[test]
fn chosen_columns_in_the_order_asked() {
    let mut t = table();
    let sql = "select id, title, org, amount, upper(org) as o from t where amount > 0 order by amount desc";
    let h = prepare(&mut t, sql).unwrap();
    let got = rows_of(h.page(&mut t, 20, 300, Some(&["o", "title", "id"])).unwrap());
    let want = query(&mut t, "select upper(org) as o, title, id from t where amount > 0 order by amount desc, id limit 280 offset 20");
    assert_eq!(got, want);
    assert_eq!(h.page(&mut t, 0, 5, Some(&[])).unwrap().out_rows, 5);
    let e = h.page(&mut t, 0, 5, Some(&["nope"])).err().unwrap();
    assert!(e.contains("no result column 'nope'"), "{e}");
}

/// A dictionary column selected as-is stays codes + the shared dictionary
/// on a page; plain text and numbers come as their columnar kinds.
#[test]
fn pages_are_columnar_and_keep_dictionaries() {
    let mut t = table();
    let h = prepare(&mut t, "select org, title, amount, id from t order by amount desc").unwrap();
    let r = h.page(&mut t, 100, 400, None).unwrap();
    let cols = r.cols.as_ref().expect("a page is columnar");
    assert!(matches!(cols[0], OutCol::Dict { .. }));
    assert!(matches!(cols[1], OutCol::Text { .. }));
    assert!(matches!(cols[2], OutCol::F64 { .. }));
    assert!(matches!(cols[3], OutCol::I64 { .. }));
    assert_eq!(r.out_rows, 300);
}

/// Grouped results are kept whole and sliced; `sort` runs the statement again.
#[test]
fn grouped_results_are_kept() {
    let mut t = table();
    let sql = "select org, year, count(*) as n, sum(amount) as total from t group by org, year order by org, year";
    let mut h = prepare(&mut t, sql).unwrap();
    assert!(!h.is_rows() && !h.has_row_ids());
    let want = query(&mut t, sql);
    assert_eq!(want.len(), 10);
    check_pages(&mut t, &h, &want, sql);
    let cat = &mut facetful_engine::sql::NoCatalog;
    h.sort(&mut t, "total desc, org, year", cat).unwrap();
    let sorted = query(&mut t, "select org, year, count(*) as n, sum(amount) as total from t group by org, year order by total desc, org, year");
    check_pages(&mut t, &h, &sorted, "grouped, sorted");
    let got = rows_of(h.page(&mut t, 3, 9, Some(&["n", "org"])).unwrap());
    let want = query(&mut t, "select count(*) as n, org from t group by org, year order by sum(amount) desc, org, year limit 6 offset 3");
    assert_eq!(got, want);
    assert!(h.sort(&mut t, "nope", cat).is_err());
    check_pages(&mut t, &h, &sorted, "grouped, after a refused sort");
    // a bare aggregate: one row
    let h = prepare(&mut t, "select count(*) as n, max(amount) as m from t").unwrap();
    let one = query(&mut t, "select count(*) as n, max(amount) as m from t");
    check_pages(&mut t, &h, &one, "bare aggregate");
}

/// Joined and derived results are not rows of the table: kept whole. The
/// join's rows come in the base table's order, columnar.
#[cfg(feature = "write")]
#[test]
fn joined_results_are_kept() {
    let mut t = table();
    for sql in [
        "with u as (select id, amount from t) select t.id, t.title, u.amount from t join u on t.id = u.id where t.year = 2002 order by t.id",
        "with big as (select id, org, amount from t where amount > 10) select org, id, amount from big order by id",
        "select id, title from t where org in (select org from t where amount > 28) order by id",
    ] {
        let mut h = prepare(&mut t, sql).unwrap_or_else(|d| panic!("{}", d.render(sql)));
        let want = query(&mut t, sql);
        assert!(want.len() > 20, "{sql}: {}", want.len());
        check_pages(&mut t, &h, &want, sql);
        h.sort(&mut t, "id desc", &mut facetful_engine::sql::NoCatalog).unwrap_or_else(|e| panic!("{sql}: {e}"));
        let sorted = query(&mut t, &sql.replace("order by t.id", "order by t.id desc").replace("order by id", "order by id desc"));
        check_pages(&mut t, &h, &sorted, &format!("{sql}, sorted"));
    }
}

/// search() in the WHERE and search_rank() as a column and as the order,
/// scanning and through the word index.
#[cfg(feature = "search")]
#[test]
fn search_and_rank() {
    for indexed in [false, true] {
        let mut t = table();
        if indexed {
            t.build_search_index(&[0, 1]).unwrap();
        }
        let rank = "search_rank('health lond', title, org)";
        let sel = format!("id, title, org, {rank} as r");
        let wh = "where search('health lond', title, org) or search('sport', title, org)";
        let sql = format!("select {sel} from t {wh} order by {rank}");
        let mut h = prepare(&mut t, &sql).unwrap_or_else(|d| panic!("{}", d.render(&sql)));
        assert!(h.is_rows());
        let want = query(&mut t, &format!("select {sel} from t {wh} order by {rank}, id"));
        assert!(want.len() > 100, "{}", want.len());
        check_pages(&mut t, &h, &want, &format!("{sql} (indexed: {indexed})"));
        let cat = &mut facetful_engine::sql::NoCatalog;
        h.sort(&mut t, "amount desc", cat).unwrap();
        let by_amount = query(&mut t, &format!("select {sel} from t {wh} order by amount desc, id"));
        check_pages(&mut t, &h, &by_amount, &format!("search sorted by amount (indexed: {indexed})"));
        h.sort(&mut t, "r desc, title", cat).unwrap();
        let by_rank = query(&mut t, &format!("select {sel} from t {wh} order by r desc, title, id"));
        check_pages(&mut t, &h, &by_rank, &format!("search sorted by rank (indexed: {indexed})"));
    }
}

/// The handle's rows are fixed when it is prepared: an index built later
/// changes what search() matches for new statements, not the handle's rows.
#[cfg(feature = "search")]
#[test]
fn rows_are_fixed_at_prepare() {
    let mut t = table();
    let sql = "select id, title from t where search('helth', title)";
    let mut h = prepare(&mut t, sql).unwrap();
    let scanned = query(&mut t, "select id, title from t where search('helth', title) order by id");
    t.build_search_index(&[0]).unwrap();
    let corrected = query(&mut t, "select id, title from t where search('helth', title) order by id");
    assert!(scanned.is_empty() && !corrected.is_empty(), "the index corrects the typo: {} / {}", scanned.len(), corrected.len());
    check_pages(&mut t, &h, &scanned, "after the index was built");
    h.sort(&mut t, "title", &mut facetful_engine::sql::NoCatalog).unwrap();
    assert_eq!(h.rows(), 0);
}
