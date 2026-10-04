//! Opening images as they arrive (`load`): images whose header gives their
//! length and images written before it did open alike.
use facetful_engine::load::TableLoad;
use facetful_engine::sql::run_query;
use facetful_engine::Table;

fn rows_of(t: &mut Table<Vec<u8>>, sql: &str) -> Vec<String> {
    let mut r = run_query(t, sql).unwrap_or_else(|d| panic!("{}", d.render(sql)));
    r.ensure_rows();
    r.rows.iter().map(|row| format!("{row:?}")).collect()
}

#[test]
fn old_and_new_images_open_as_they_arrive() {
    // data-1000000 predates the length in the header; data-200000 has it
    for (file, has_len) in [("data-1000000.facetful", false), ("data-200000.facetful", true)] {
        let bytes = std::fs::read(format!("{}/../../spikes/facet-spike/{file}", env!("CARGO_MANIFEST_DIR"))).unwrap();
        let mut whole = Table::open(bytes.clone()).unwrap();
        assert_eq!(whole.catalog().image_len, has_len.then_some(bytes.len() as u64), "{file}");
        let mut load = TableLoad::<Vec<u8>>::new(0);
        for c in bytes.chunks(1 << 20) {
            load.push(c).unwrap();
        }
        assert_eq!(load.groups_ready(), whole.group_count(), "{file}");
        let mut t = load.finish().unwrap();
        for sql in [
            "select count(*), count(capacity), count(distinct owner) from t",
            "select country, count(*) as n, sum(capacity) from t group by country order by n desc, country limit 5",
            "select id from t where capacity > 100 order by capacity desc, id limit 5",
        ] {
            assert_eq!(rows_of(&mut t, sql), rows_of(&mut whole, sql), "{file}: {sql}");
        }
    }
}

fn fixture(file: &str) -> Vec<u8> {
    std::fs::read(format!("{}/../../spikes/facet-spike/{file}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

/// An image split in two, the second attached to the first: the same table
/// to every query, whichever part its columns come from.
#[test]
fn split_parts_attach_into_one_table() {
    use facetful_engine::format::split::split;
    let bytes = fixture("data-200000.facetful");
    let mut whole = Table::open(bytes.clone()).unwrap();
    let names: Vec<String> = whole.catalog().schema.columns.iter().map(|c| c.name.clone()).collect();
    let second: Vec<usize> = ["owner", "capacity", "fuel"].iter().map(|n| whole.column_index(n).unwrap()).collect();
    let src: &[u8] = &bytes;
    let (a, b) = split(&src, &second, 42).unwrap();
    let mut t = Table::open(a.clone()).unwrap();
    // before the second part: its columns are unknown
    assert!(run_query(&mut t, "select count(*) from t where owner = 'owner_1'").is_err());
    assert_eq!(rows_of(&mut t, "select count(*) from t where country = 'country_3'"), rows_of(&mut whole, "select count(*) from t where country = 'country_3'"));
    t.attach(Table::open(b.clone()).unwrap()).unwrap();
    // every column under its old name; the first part's indices unchanged
    for n in &names {
        assert!(t.column_index(n).is_some(), "{n}");
    }
    assert_eq!(t.column_index("country"), Some(0));
    let all = names.iter().map(|n| format!("\"{n}\"")).collect::<Vec<_>>().join(", ");
    for sql in [
        format!("select {all} from t order by id, capacity limit 50"),
        "select count(*), count(capacity), count(distinct owner), sum(capacity) from t".into(),
        "select fuel, country, count(*) as n, round(sum(capacity), 3) from t group by fuel, country order by n desc, fuel, country limit 20".into(),
        "select id, owner from t where capacity > 250 and status = 'status_1' order by capacity desc, id limit 20".into(),
        "select count(*) from t where owner like 'owner_1%' or region = 'region_2'".into(),
    ] {
        assert_eq!(rows_of(&mut t, &sql), rows_of(&mut whole, &sql), "{sql}");
    }
    // the parts also stream in
    let mut la = TableLoad::<Vec<u8>>::new(0);
    for c in a.chunks(4096) {
        la.push(c).unwrap();
    }
    let mut lb = TableLoad::<Vec<u8>>::new(0);
    for c in b.chunks(4096) {
        lb.push(c).unwrap();
    }
    let mut s = la.finish().unwrap();
    s.attach(lb.finish().unwrap()).unwrap();
    let sql = "select fuel, count(*), round(sum(capacity), 3) from t group by fuel order by fuel";
    assert_eq!(rows_of(&mut s, sql), rows_of(&mut whole, sql));
    // parts of different splits, different row groups, or no split: refused
    let (_, b2) = split(&src, &second, 43).unwrap();
    assert!(Table::open(a.clone()).unwrap().attach(Table::open(b2).unwrap()).is_err());
    let small = fixture("data-1000000.facetful");
    let small_src: &[u8] = &small;
    let (_, b3) = split(&small_src, &[0], 42).unwrap();
    assert!(Table::open(a.clone()).unwrap().attach(Table::open(b3).unwrap()).is_err());
    assert!(Table::open(bytes.clone()).unwrap().attach(Table::open(b.clone()).unwrap()).is_err());
    // nor a part whose names clash with the table's
    let (a4, _) = split(&src, &second, 44).unwrap();
    let (a5, _) = split(&src, &second, 44).unwrap();
    assert!(Table::open(a4).unwrap().attach(Table::open(a5).unwrap()).is_err());
}
