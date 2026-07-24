//! Field diagnostic: dump a share's persisted per-path sync index (the "base"
//! the reconcile merges against) and compare it to what is on disk right now.
//!
//! A path whose on-disk hash differs from its base hash is content the engine
//! has not reconciled — if the engine also thinks the folder is settled, that is
//! the known-issues #30 shape.
//!
//!   cargo run -p seed-core --example showindex -- <state.db> <share_id> [path-filter]

use std::collections::HashMap;

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let (Some(db_path), Some(share)) = (args.next(), args.next()) else {
        eprintln!("usage: showindex <state.db> <share_id> [path-filter]");
        std::process::exit(2);
    };
    let filter = args.next();

    let db = seed_core::db::Db::open(std::path::Path::new(&db_path))?;
    let index: HashMap<String, Vec<u8>> = db.get_index(&share)?;
    let rec = db.load_all()?.into_iter().find(|s| s.share_id == share);
    let folder = rec.as_ref().map(|s| s.folder.clone()).unwrap_or_default();

    println!("share     {share}");
    println!("folder    {folder}");
    println!(
        "quick_sig {:016x}   (the folder signature the engine thinks is settled)",
        rec.as_ref().map(|s| s.quick_sig).unwrap_or(0)
    );
    println!("index     {} path(s)\n", index.len());

    let mut rows: Vec<(&String, &Vec<u8>)> = index.iter().collect();
    rows.sort();
    for (path, base) in rows {
        if let Some(f) = &filter {
            if !path.contains(f.as_str()) {
                continue;
            }
        }
        let abs = std::path::Path::new(&folder).join(path.replace('/', "\\"));
        let disk = seed_core::scan::hash_file(&abs).ok();
        let state = match &disk {
            None => "MISSING/UNREADABLE".to_string(),
            Some((h, _)) if h == base => "match".to_string(),
            Some((h, _)) => format!("DIFFERS  disk={}", hex(h)),
        };
        println!("{path}\n  base={}  {state}", hex(base));
    }
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
