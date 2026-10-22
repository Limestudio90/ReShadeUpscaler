//! Incorpora l'icona (assets\app.ico, risorsa 1) nell'exe con rc.exe del Windows SDK.

use std::path::PathBuf;
use std::process::Command;

fn find_rc() -> Option<PathBuf> {
    if let Ok(rc) = std::env::var("RC") {
        return Some(rc.into());
    }
    let kits = PathBuf::from(r"C:\Program Files (x86)\Windows Kits\10\bin");
    let mut versions: Vec<PathBuf> = std::fs::read_dir(&kits).ok()?.flatten().map(|e| e.path()).collect();
    versions.sort();
    versions.into_iter().rev().map(|v| v.join(r"x64\rc.exe")).find(|p| p.exists())
}

fn main() {
    println!("cargo:rerun-if-changed=assets/app.rc");
    println!("cargo:rerun-if-changed=assets/app.ico");
    let Some(rc) = find_rc() else {
        println!("cargo:warning=rc.exe non trovato: exe senza icona");
        return;
    };
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("app.res");
    let ok = Command::new(rc)
        .args(["/nologo", "/fo"])
        .arg(&out)
        .arg("assets/app.rc")
        .status()
        .is_ok_and(|s| s.success());
    if ok {
        println!("cargo:rustc-link-arg-bins={}", out.display());
    } else {
        println!("cargo:warning=rc.exe fallito: exe senza icona");
    }
}
