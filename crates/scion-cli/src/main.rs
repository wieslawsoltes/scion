use clap::Parser;
fn main() {
    let args = scion_cli::Args::parse();
    let result = scion_cli::try_run_local(&args).unwrap_or_else(|| {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        runtime.block_on(scion_cli::run(&args))
    });
    if let Err(error) = result {
        if args.json {
            eprintln!("{}", serde_json::json!({"error": error.to_string()}));
        } else {
            eprintln!("scn: {error}");
        }
        std::process::exit(match error {
            scion_types::Error::Conflict(_) => 2,
            scion_types::Error::Permission(_) => 3,
            scion_types::Error::NotFound(_) => 4,
            _ => 1,
        });
    }
}
