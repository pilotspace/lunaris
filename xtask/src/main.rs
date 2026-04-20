fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("help") | None => {
            println!("xtask — Lunaris dev tasks");
            println!();
            println!("usage: cargo xtask <subcommand>");
            println!();
            println!("subcommands:");
            println!("  help    Print this help (default).");
            println!();
            println!("(More subcommands land in later phases: codegen, bench, eval-smoke.)");
            Ok(())
        }
        Some(other) => {
            anyhow::bail!("unknown xtask subcommand: {other}");
        }
    }
}
