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
