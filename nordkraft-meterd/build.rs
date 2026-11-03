//! On macOS a bare command-line binary is killed by the system the moment it
//! touches CoreBluetooth unless it carries an Info.plist with a Bluetooth
//! usage description. Embedding one in the executable (Apple's documented
//! way for single-file tools) turns that crash into the normal permission
//! prompt, so `scan` and `probe` can be run from a Mac during setup.
//! Linux builds are untouched.

fn main() {
    println!("cargo:rerun-if-changed=Info.plist");
    println!("cargo:rerun-if-changed=build.rs");
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os == "macos" {
        let plist = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("Info.plist");
        println!(
            "cargo:rustc-link-arg-bins=-Wl,-sectcreate,__TEXT,__info_plist,{}",
            plist.display()
        );
    }
}
