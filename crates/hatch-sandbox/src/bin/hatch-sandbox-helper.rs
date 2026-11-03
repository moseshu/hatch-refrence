//! Standalone sandbox helper (the `hatch` binary embeds the same code behind
//! its hidden `__sandbox-helper` subcommand).

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    hatch_sandbox::helper_main(&args)
}
