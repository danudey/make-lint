//! Print the resolved variable table, for differential testing against
//! `make -p`. Not part of the linter's interface.
use make_lint::{eval, workspace::Workspace};

fn main() {
    let path = std::env::args().nth(1).unwrap_or_else(|| "Makefile".into());
    let mut ws = Workspace::new(Vec::new());
    ws.load_root(std::path::Path::new(&path)).unwrap();
    let a = eval::analyse(&ws);
    for (name, v) in &a.vars {
        match v.value.as_known() {
            Some(t) => println!("{name} = {t}"),
            None => println!(
                "{name} ?= <{}>",
                v.value.first_unknown().map(|u| u.reason.describe()).unwrap_or_default()
            ),
        }
    }
}
