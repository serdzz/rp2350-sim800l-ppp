//! Copies `memory.x` into `OUT_DIR` and puts it on the linker search path,
//! then passes the linker scripts the RP2350 image needs.

use std::env;
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;

fn main() {
    let out = &PathBuf::from(env::var_os("OUT_DIR").unwrap());
    File::create(out.join("memory.x"))
        .unwrap()
        .write_all(include_bytes!("memory.x"))
        .unwrap();
    println!("cargo:rustc-link-search={}", out.display());
    println!("cargo:rerun-if-changed=memory.x");

    println!("cargo:rustc-link-arg-bins=--nmagic");
    println!("cargo:rustc-link-arg-bins=-Tlink.x");

    // defmt.x генерирует build-скрипт крейта defmt. В сборке с USB-логгером
    // defmt не подключён, и скрипта попросту нет — линкер упадёт, если его
    // требовать безусловно.
    if env::var_os("CARGO_FEATURE__DEFMT").is_some() {
        println!("cargo:rustc-link-arg-bins=-Tdefmt.x");
    }
}
