//! Field diagnostic: print the BLAKE3 content hash a share would publish for a
//! file, so it can be compared against what the blob store / replica actually
//! holds. Same code path as the scanner (`scan::hash_file`).
//!
//!   cargo run -p seed-core --example hashfile -- "D:\SEED_Share\some.iso"

fn main() {
    let mut args = std::env::args_os().skip(1);
    let Some(path) = args.next() else {
        eprintln!("usage: hashfile <path>");
        std::process::exit(2);
    };
    let path = std::path::PathBuf::from(path);
    match seed_core::scan::hash_file(&path) {
        Ok((hash, size)) => println!("{}  {size} bytes  {}", hex(&hash), path.display()),
        Err(e) => {
            eprintln!("{}: {e}", path.display());
            std::process::exit(1);
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
