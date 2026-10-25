use clap::Parser;
#[tokio::main]
async fn main() {
    let args = scion_cli::Args::parse();
    if let Err(error) = scion_cli::run(&args).await {
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
