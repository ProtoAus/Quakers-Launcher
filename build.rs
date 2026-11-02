// P1212: Windows version info (publisher, product, file version) for WOS-Launcher.exe, so Explorer,
// SmartScreen and antivirus see who made it. MSVC targets only; no crates, so Cargo.lock and the
// offline Linux bootstrap are unchanged. The resource is linked into WOS-Launcher alone.
use std::{env, fs, path::PathBuf, process::Command};

fn rc_exe() -> Option<PathBuf> {
    if let Ok(dir) = env::var("WindowsSdkVerBinPath") {
        let p = PathBuf::from(dir).join("x64").join("rc.exe");
        if p.is_file() {
            return Some(p);
        }
    }
    let kits = PathBuf::from(r"C:\Program Files (x86)\Windows Kits\10\bin");
    let mut versions: Vec<PathBuf> = fs::read_dir(&kits)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.join("x64").join("rc.exe").is_file())
        .collect();
    versions.sort();
    versions.pop().map(|p| p.join("x64").join("rc.exe"))
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let target = env::var("TARGET").unwrap_or_default();
    if !target.ends_with("-windows-msvc") {
        return;
    }
    let version = env::var("CARGO_PKG_VERSION").unwrap();
    let mut n = version.split('.').map(|s| s.parse::<u16>().unwrap_or(0));
    let (a, b, c) = (n.next().unwrap_or(0), n.next().unwrap_or(0), n.next().unwrap_or(0));
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let rc = out.join("wos-launcher.rc");
    let res = out.join("wos-launcher.res");
    fs::write(
        &rc,
        format!(
            "1 VERSIONINFO\n FILEVERSION {a},{b},{c},0\n PRODUCTVERSION {a},{b},{c},0\n FILEOS 0x40004\n FILETYPE 0x1\n\
             BEGIN\n BLOCK \"StringFileInfo\"\n BEGIN\n  BLOCK \"040904b0\"\n  BEGIN\n\
             \x20  VALUE \"CompanyName\", \"World of Surf\"\n\
             \x20  VALUE \"FileDescription\", \"World of Surf Launcher\"\n\
             \x20  VALUE \"FileVersion\", \"{version}\"\n\
             \x20  VALUE \"InternalName\", \"WOS-Launcher\"\n\
             \x20  VALUE \"LegalCopyright\", \"MIT License, quakers-launcher contributors\"\n\
             \x20  VALUE \"OriginalFilename\", \"WOS-Launcher.exe\"\n\
             \x20  VALUE \"ProductName\", \"World of Surf\"\n\
             \x20  VALUE \"ProductVersion\", \"{version}\"\n\
             \x20 END\n END\n BLOCK \"VarFileInfo\"\n BEGIN\n  VALUE \"Translation\", 0x409, 1200\n END\nEND\n"
        ),
    )
    .unwrap();
    let exe = rc_exe().expect("rc.exe (Windows SDK) is needed for the WOS-Launcher version resource");
    let status = Command::new(exe)
        .args(["/nologo", "/fo"])
        .arg(&res)
        .arg(&rc)
        .status()
        .expect("could not run rc.exe");
    assert!(status.success(), "rc.exe failed on {}", rc.display());
    println!("cargo:rustc-link-arg-bin=WOS-Launcher={}", res.display());
}
