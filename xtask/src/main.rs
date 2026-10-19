//! Dev tasks: schema bump, bench run. Stub until the schema changes.

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("schema") => eprintln!("schema bump not implemented yet (schema is v1)"),
        Some(other) => {
            eprintln!("unknown task: {other}");
            std::process::exit(2);
        }
        None => eprintln!("usage: xtask <task>"),
    }
}
