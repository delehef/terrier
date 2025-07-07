use std::path::Path;

use anyhow::{Context, ensure};
use clap::Parser;
use dialoguer::FuzzySelect;
use fern::colors::{Color, ColoredLevelConfig};
use indexing::Index;
use spinoff::{Spinner, spinners};
use tabled::tables::IterTable;

mod indexing;

#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// Root of the project to analyze.
    #[arg(short = 'R', long, default_value = ".")]
    root: String,

    /// The rust-analyzer binary to use
    #[arg(short, long, default_value = "rust-analyzer")]
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
    let root = Path::new(&args.root).canonicalize()?;
    ensure!(root.is_dir(), "`{}` is not a directory", root.display());

    let mut indexer = Index::new(&root, args.clutter.into_iter().collect(), &args.ra_bin).await?;

    // Find all functions in project
    let mut spinner = Spinner::new(
        spinners::Dots,
        "Indexing functions...",
        spinoff::Color::Blue,
    );

    let function_names = indexer
        .functions
        .iter()
        .map(|f| f.pretty(&root))
        .collect::<Vec<_>>();
    spinner.stop_and_persist("", "Functions indexed");

    while let Some(f_id) = FuzzySelect::new()
        .items(&function_names)
        .max_length(15)
        .interact_opt()?
    {
        let mut spinner = Spinner::new(spinners::Dots, "Generating...", spinoff::Color::Blue);
        let (incomings, outgoings) = indexer.context(f_id).await?;
        spinner.clear();

        let header = [
            "".to_string(),
            "".to_string(),
            indexer.functions[f_id].pretty(&root),
            "".to_string(),
            "".to_string(),
        ];

        let content = std::iter::once(header)
            .chain(
                incomings
                    .iter()
                    .filter(|i| Path::new(i.0.uri.path()).starts_with(&root))
                    .map(|i| {
                        [
                            i.pretty(&root),
                            "--->".to_string(),
                            "".into(),
                            "".into(),
                            "".into(),
                        ]
                    }),
            )
            .chain(
                outgoings
                    .iter()
                    .filter(|o| Path::new(o.0.uri.path()).starts_with(&root))
                    .map(|o| {
                        [
                            "".into(),
                            "".into(),
                            "".into(),
                            "--->".to_string(),
                            o.pretty(&root),
                        ]
                    }),
            );

        let table = IterTable::new(content);
        let o = table.to_string();

        println!("{o}");
    }

    indexer.shutdown().await.context("shutting down indexer")
}
