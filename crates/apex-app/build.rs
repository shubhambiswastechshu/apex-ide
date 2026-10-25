//! Compiles the Slint UI (`ui/app.slint` and everything it imports) into
//! Rust code that `slint::include_modules!()` pulls in.

fn main() {
    let config = slint_build::CompilerConfiguration::new().with_style("fluent".into());
    if let Err(err) = slint_build::compile_with_config("ui/app.slint", config) {
        // A failing build script must stop the build; print the compiler
        // diagnostics instead of a panic backtrace.
        eprintln!("failed to compile the Slint UI: {err}");
        std::process::exit(1);
    }
}
