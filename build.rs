fn main() {
    #[cfg(target_os = "macos")]
    {
        // Embed Info.plist into the binary
        println!("cargo:rustc-link-arg=-Wl,-sectcreate,__TEXT,__info_plist,Info.plist");
        println!("cargo:rustc-link-arg=-Wl,-sectcreate,__TEXT,__info_plist,Info.plist");
        
        // Tell cargo to re-run this build script if Info.plist changes
        println!("cargo:rerun-if-changed=Info.plist");
    }
}
