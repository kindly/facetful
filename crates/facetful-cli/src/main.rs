//! `facetful` CLI. M1 spike scope: `convert` (CSV -> .facetful) and `inspect`.
fn main() {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("inspect") => todo!("M1: inspect"),
        Some("convert") => todo!("M1: convert"),
        _ => {
            eprintln!("usage: facetful <convert|inspect> ...");
            std::process::exit(2);
        }
    }
}
