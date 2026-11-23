fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    // In debug builds, the top-level async runner can exceed the Windows
    // linker's default 1 MiB stack before it has polled a provider request.
    let target_env = std::env::var("CARGO_CFG_TARGET_ENV");
    let stack = match target_env.as_deref() {
        Ok("msvc") => "/STACK:8388608",
        Ok("gnu") => "-Wl,--stack,8388608",
        _ => return,
    };
    println!("cargo:rustc-link-arg-bin=ra={stack}");
}
