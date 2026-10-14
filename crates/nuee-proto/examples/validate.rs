//! Validates task modules: `cargo run -q -p nuee-proto --example validate -- <file.wasm>...`.

use std::process::ExitCode;

fn main() -> ExitCode {
    let mut failed = false;
    for path in std::env::args().skip(1) {
        let name = std::path::Path::new(&path)
            .file_name()
            .map_or_else(|| path.clone(), |n| n.to_string_lossy().into_owned());
        match std::fs::read(&path) {
            Ok(bytes) => match nuee_proto::validate_module(&bytes) {
                Ok(info) => println!(
                    "{name} ok hash={} size={} max_memory={}",
                    info.hash, info.size, info.max_memory
                ),
                Err(e) => {
                    println!("{name} error: {e}");
                    failed = true;
                }
            },
            Err(e) => {
                println!("{name} error: cannot read: {e}");
                failed = true;
            }
        }
    }
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}
