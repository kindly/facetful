//! User-defined functions through a Rust host: binding (types, arity,
//! suggestions), evaluation over lanes (numbers, text, bools, NULLs,
//! broadcast literals), the dictionary path, and a failing body.

use facetful_engine::format::write::{ColumnChunk, DictData, SegmentData, Writer};
use facetful_engine::format::{flags, ColumnDef, ColumnType, Schema};
use facetful_engine::sql::binder::Ty;
use facetful_engine::sql::run_query;
use facetful_engine::udf::{self, Arg, Host, Kind, Lane, Out, Output};
use facetful_engine::Table;

/// 6 rows: name (dict, row 4 NULL), n (int, row 2 NULL), x (float)
fn table() -> Table<Vec<u8>> {
    let schema = Schema { columns: vec![
        ColumnDef { name: "name".into(), ty: ColumnType::Utf8, flags: flags::DICTIONARY | flags::CODES_U8 },
        ColumnDef { name: "n".into(), ty: ColumnType::Int32, flags: 0 },
        ColumnDef { name: "x".into(), ty: ColumnType::Float64, flags: 0 },
    ] };
    let mut offs = vec![0u32];
    let mut bytes = Vec::new();
    for s in ["Sand Point", "Coal Creek", "Wind Farm", "Barry"] {
        bytes.extend_from_slice(s.as_bytes());
        offs.push(bytes.len() as u32);
    }
    let dicts = vec![Some(DictData { offsets: offs, bytes }), None, None];
    let mut w = Writer::new(schema, vec![], 6, &dicts);
    let codes: [u8; 6] = [0, 1, 2, 3, 0, 1];
    let ns: [i32; 6] = [1, 2, 0, 4, 5, 6];
    let xs: [f64; 6] = [0.5, 1.5, 2.5, 3.5, 4.5, 5.5];
    let name_valid: [u8; 1] = [0b0010_1111];
    let n_valid: [u8; 1] = [0b0011_1011];
    let nb: Vec<u8> = ns.iter().flat_map(|v| v.to_le_bytes()).collect();
    let xb: Vec<u8> = xs.iter().flat_map(|v| v.to_le_bytes()).collect();
    w.write_group(6, &[
        ColumnChunk { data: SegmentData::Codes8(&codes), validity: Some(&name_valid), null_count: 1 },
        ColumnChunk { data: SegmentData::Fixed(&nb), validity: Some(&n_valid), null_count: 1 },
        ColumnChunk { data: SegmentData::Fixed(&xb), validity: None, null_count: 0 },
    ]);
    Table::open(w.finish()).unwrap()
}

fn text<'a>(a: &'a Arg, i: usize) -> &'a str {
    let Lane::Text { offsets, bytes } = &a.lane else { panic!("text lane expected") };
    let i = if a.broadcast { 0 } else { i };
    std::str::from_utf8(&bytes[offsets[i] as usize..offsets[i + 1] as usize]).unwrap()
}
fn num(a: &Arg, i: usize) -> f64 {
    let Lane::Num(v) = &a.lane else { panic!("numeric lane expected") };
    v[if a.broadcast { 0 } else { i }]
}
fn valid(a: &Arg, i: usize) -> bool {
    let i = if a.broadcast { 0 } else { i };
    a.valid.map_or(true, |b| b[i / 8] >> (i % 8) & 1 == 1)
}

/// The host: dispatches on the id handed back by register().
struct TestHost { ids: std::collections::HashMap<u32, &'static str>, calls: std::rc::Rc<std::cell::Cell<usize>>, lens: std::rc::Rc<std::cell::RefCell<Vec<usize>>> }
impl Host for TestHost {
    fn call(&mut self, id: u32, args: &[Arg], len: usize, out: &mut Output) -> Result<(), String> {
        self.calls.set(self.calls.get() + 1);
        self.lens.borrow_mut().push(len);
        match self.ids[&id] {
            "plus" => {
                let Out::Num(o) = &mut out.out else { unreachable!() };
                for i in 0..len { o[i] = num(&args[0], i) + num(&args[1], i); }
            }
            "shout" => {
                let Out::Text { offsets, bytes } = &mut out.out else { unreachable!() };
                for i in 0..len {
                    bytes.extend_from_slice(text(&args[0], i).to_uppercase().as_bytes());
                    offsets[i + 1] = bytes.len() as u32;
                }
            }
            "has" => {
                let Out::Bool(o) = &mut out.out else { unreachable!() };
                for i in 0..len { o[i] = text(&args[0], i).contains(text(&args[1], i)) as u8; }
            }
            // non-strict: sees NULLs, turns them into 0 and marks the rest
            "zero_if_null" => {
                let Out::Num(o) = &mut out.out else { unreachable!() };
                for i in 0..len { o[i] = if valid(&args[0], i) { num(&args[0], i) } else { 0.0 }; }
            }
            "boom" => return Err("kaboom".into()),
            "count_args" => {
                let Out::Num(o) = &mut out.out else { unreachable!() };
                for i in 0..len { o[i] = args.len() as f64; }
            }
            _ => unreachable!(),
        }
        Ok(())
    }
}

fn setup() -> (std::rc::Rc<std::cell::Cell<usize>>, std::rc::Rc<std::cell::RefCell<Vec<usize>>>) {
    let mut ids = std::collections::HashMap::new();
    ids.insert(udf::register("plus", &[Ty::Int, Ty::Int], Ty::Int, true, false).unwrap(), "plus");
    ids.insert(udf::register("shout", &[Ty::Text], Ty::Text, true, false).unwrap(), "shout");
    ids.insert(udf::register("has", &[Ty::Text, Ty::Text], Ty::Bool, true, false).unwrap(), "has");
    ids.insert(udf::register("zero_if_null", &[Ty::Float], Ty::Float, false, false).unwrap(), "zero_if_null");
    ids.insert(udf::register("boom", &[Ty::Int], Ty::Int, true, false).unwrap(), "boom");
    ids.insert(udf::register("count_args", &[Ty::Int], Ty::Int, true, true).unwrap(), "count_args");
    let calls = std::rc::Rc::new(std::cell::Cell::new(0));
    let lens = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    udf::set_host(Box::new(TestHost { ids, calls: calls.clone(), lens: lens.clone() }));
    (calls, lens)
}

fn q(t: &mut Table<Vec<u8>>, sql: &str) -> Vec<Vec<String>> {
    let mut r = run_query(t, sql).unwrap_or_else(|d| panic!("{}", d.render(sql)));
    r.ensure_rows();
    r.rows.iter().map(|row| row.iter().map(|v| format!("{v:?}")).collect()).collect()
}

#[test]
fn binding_and_lanes() {
    let (_calls, lens) = setup();
    let mut t = table();
    // ints in, int out; strict: the NULL n stays NULL
    assert_eq!(q(&mut t, "select plus(n, 10) from t order by x"),
        vec![vec!["Int(11)"], vec!["Int(12)"], vec!["Null"], vec!["Int(14)"], vec!["Int(15)"], vec!["Int(16)"]]);
    // a float literal coerces where the parameter is int? no: int param, float arg is refused
    let err = run_query(&mut t, "select plus(x, 1) from t").err().unwrap().render("");
    assert!(err.contains("plus() argument 1 needs int, this is float"), "{err}");
    // text out, over the dictionary (one call over 4 values, gathered through codes; NULL name stays NULL)
    lens.borrow_mut().clear();
    assert_eq!(q(&mut t, "select shout(name) from t order by x"),
        vec![vec!["Text(\"SAND POINT\")"], vec!["Text(\"COAL CREEK\")"], vec!["Text(\"WIND FARM\")"], vec!["Text(\"BARRY\")"], vec!["Null"], vec!["Text(\"COAL CREEK\")"]]);
    assert_eq!(lens.borrow().as_slice(), &[4], "dictionary path: the lane is the dictionary");
    // bool out in WHERE with a broadcast literal, over the dictionary; then a non-literal second arg forces the row path
    lens.borrow_mut().clear();
    assert_eq!(q(&mut t, "select count(*) from t where has(name, 'Cree')"), vec![vec!["Int(2)"]]);
    assert_eq!(lens.borrow().as_slice(), &[4]);
    lens.borrow_mut().clear();
    assert_eq!(q(&mut t, "select count(*) from t where has(name, shout(name))"), vec![vec!["Int(0)"]]);
    assert!(lens.borrow().iter().all(|&l| l == 6 || l == 4), "{:?}", lens.borrow());
    // the mask cache serves the predicate on a rerun: no host call
    let (calls, _) = setup();
    q(&mut t, "select count(*) from t where has(name, 'Cree')");
    let after_first = calls.get();
    q(&mut t, "select count(*) from t where has(name, 'Cree')");
    assert_eq!(calls.get(), after_first, "cached conjunct: the host is not called again");
    // non-strict sees the NULL
    assert_eq!(q(&mut t, "select sum(zero_if_null(n)) from t"), vec![vec!["Float(18.0)"]]);
    // variadic: the last parameter repeats
    assert_eq!(q(&mut t, "select count_args(1, 2, 3) from t limit 1"), vec![vec!["Int(3)"]]);
    let err = run_query(&mut t, "select count_args(1, 'x') from t").err().unwrap().render("");
    assert!(err.contains("argument 2 needs int"), "{err}");
    // grouped expression path: a UDF over an aggregate
    assert_eq!(q(&mut t, "select plus(max(n), 1) from t"), vec![vec!["Int(7)"]]);
}

#[test]
fn errors_and_registry() {
    setup();
    let mut t = table();
    let err = run_query(&mut t, "select boom(n) from t").err().unwrap().render("");
    assert!(err.contains("user-defined function failed: kaboom"), "{err}");
    // arity
    let err = run_query(&mut t, "select plus(n) from t").err().unwrap().render("");
    assert!(err.contains("plus() takes 2 argument(s)"), "{err}");
    // unknown function suggests a registered one
    let err = run_query(&mut t, "select shoutt(name) from t").err().unwrap().render("");
    assert!(err.contains("did you mean 'shout()'"), "{err}");
    // built-in names are protected; bad names refused
    assert!(udf::register("upper", &[Ty::Text], Ty::Text, true, false).is_err());
    assert!(udf::register("no-dash", &[Ty::Text], Ty::Text, true, false).is_err());
    // unregister
    assert!(udf::unregister("boom"));
    assert!(!udf::unregister("boom"));
    let err = run_query(&mut t, "select boom(n) from t").err().unwrap().render("");
    assert!(err.contains("unknown function 'boom'"), "{err}");
    // kinds round-trip
    for k in 0..6u8 {
        let kind = Kind::from_u8(k).unwrap();
        assert_eq!(Kind::of(kind.ty()), kind);
    }
}
