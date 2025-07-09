use std::path::PathBuf;

use anyhow::ensure;
use clap::Parser;
use fern::colors::{Color, ColoredLevelConfig};
use indexing::Index;

mod indexing;
mod ui;

#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// Root of the project to analyze.
    #[arg(short = 'R', long, default_value = ".")]
    root: PathBuf,

    /// The rust-analyzer binary to use
    #[arg(long, default_value = "rust-analyzer")]
    ra_bin: String,

    /// A list of function names to ignore
    #[arg(long, default_values_t = ["into_iter".to_string(), "iter".to_string(), "clone".to_string()])]
    clutter: Vec<String>,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let colors = ColoredLevelConfig::new().info(Color::Green);
    fern::Dispatch::new()
        .format(move |out, message, record| {
            out.finish(format_args!(
                "[{}] {}",
                colors.color(record.level()),
                message
            ))
        })
        .level(log::LevelFilter::Trace)
        .chain(std::io::stdout())
        .apply()?;

    let args = Args::parse();
    let root = args.root.canonicalize()?;
    ensure!(root.is_dir(), "`{}` is not a directory", root.display());

    let indexer = Index::new(&root, args.clutter.into_iter().collect(), &args.ra_bin).await?;
    ui::Ui::new(indexer)?.run().await
}
