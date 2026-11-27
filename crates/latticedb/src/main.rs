//! LatticeDB Host binary: assembly + config (ARCHITECTURE §3). One process —
//! owns all listeners; kernel crates run on dedicated OS thread pools (ARCH §8
//! execution model).
//!
//! v0 surface: `--config <path>` validates a host config file;
//! `--autodetect` prints a tuned config set for this machine (SPEC-02 §10).

use std::fs;
use std::process::ExitCode;

use latticedb::config::autodetect::{DiskClass, HostSpecs, autodetect};
use latticedb::config::parse_config;
use latticedb::smoke;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args.as_slice() {
        ["--config", path] => match fs::read_to_string(path)
            .map_err(|e| e.to_string())
            .and_then(|t| parse_config(&t).map_err(|e| e.message))
        {
            Ok(_) => {
                println!("config OK: {path}");
                ExitCode::SUCCESS
            }
            Err(msg) => {
                eprintln!("config INVALID: {path}\n{msg}");
                ExitCode::FAILURE
            }
        },
        ["--autodetect"] => {
            let specs = probe_host();
            match autodetect(&specs) {
                Ok(text) => {
                    print!("{text}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("autodetect failed: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        ["--smoke", dir] => match smoke::run(dir) {
            Ok(summary) => {
                println!(
                    "smoke OK: snapshot {} bytes, part {} bytes, last_lsn {} — two databases byte-identical, restart verified",
                    summary.snapshot_len, summary.part_len, summary.last_lsn
                );
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("smoke FAILED: {e}");
                ExitCode::FAILURE
            }
        },
        _ => {
            println!(
                "latticedb {} — usage:\n  latticedb --config <path.toml>   validate a host config\n  latticedb --autodetect            print a tuned config for this host\n  latticedb --smoke <dir>           run the M1 exit-criteria workload twice and verify",
                env!("CARGO_PKG_VERSION")
            );
            ExitCode::SUCCESS
        }
    }
}

/// Host probe skeleton (T-001): CPU + RAM from the OS; disk class defaults to
/// the conservative `Hdd` until M4 packaging lands real probing
/// (see `config::autodetect` module docs).
fn probe_host() -> HostSpecs {
    let cpu_cores = std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(1)
        .max(1);
    HostSpecs {
        cpu_cores,
        ram_gib: probe_ram_gib(),
        disk_class: DiskClass::Hdd,
    }
}

fn probe_ram_gib() -> u32 {
    let Ok(text) = fs::read_to_string("/proc/meminfo") else {
        return 1;
    };
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            let kib: u64 = rest
                .trim()
                .trim_end_matches("kB")
                .trim()
                .parse()
                .unwrap_or(0);
            return u32::try_from(kib / 1024 / 1024).unwrap_or(1).max(1);
        }
    }
    1
}
