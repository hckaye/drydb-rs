//! Rust half of the interop harness.
//!
//!   drydb-oracle-rs build <spec.json> <out.drydb>
//!   drydb-oracle-rs dump  <spec.json> <db.drydb> <out.json>

use std::path::Path;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.as_slice() {
        [command, spec, out] if command == "build" => build(Path::new(spec), Path::new(out)),
        [command, spec, db, out] if command == "dump" => {
            dump(Path::new(spec), Path::new(db), Path::new(out))
        }
        _ => {
            eprintln!("usage: drydb-oracle-rs build <spec> <out> | dump <spec> <db> <out>");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}

fn read_spec(path: &Path) -> Result<drydb_interop::Spec, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read {path:?}: {e}"))?;
    serde_json::from_str(&text).map_err(|e| format!("cannot parse {path:?}: {e}"))
}

fn build(spec_path: &Path, out: &Path) -> Result<(), String> {
    let spec = read_spec(spec_path)?;
    drydb_interop::build(&spec, out).map_err(|e| format!("build failed: {e}"))
}

fn dump(spec_path: &Path, db_path: &Path, out: &Path) -> Result<(), String> {
    let spec = read_spec(spec_path)?;
    let db = drydb::Database::open(db_path).map_err(|e| format!("open failed: {e}"))?;
    let dump = drydb_interop::dump(&spec, &db).map_err(|e| format!("dump failed: {e}"))?;
    let text = serde_json::to_string(&dump).map_err(|e| format!("cannot serialise: {e}"))?;
    std::fs::write(out, text).map_err(|e| format!("cannot write {out:?}: {e}"))
}
