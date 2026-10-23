//! Every program built here loads the DLLs it imports from System32 only, never from its own folder, which
//! anyone can write to (player\target\release). Whatever runs as administrator (to read PawnIO's sensors,
//! the `wvplay pawnio` probe, a player someone starts as administrator) would otherwise load a DLL planted
//! there with those rights. With the C runtime linked in (.cargo/config.toml), every DLL they import is
//! Windows' own. tests/pe.rs checks both.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        // 0x800 is LOAD_LIBRARY_SEARCH_SYSTEM32, for the imports Windows loads before the program runs
        println!("cargo:rustc-link-arg-bins=/DEPENDENTLOADFLAG:0x800");
    }
}
