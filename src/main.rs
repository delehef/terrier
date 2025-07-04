use std::{
    collections::HashSet, io::stderr, path::Path, process::Stdio
};

use anyhow::{Context, ensure};
use async_lsp::{
    LanguageServer,
    lsp_types::{
        CallHierarchyIncomingCall, CallHierarchyItem,
        CallHierarchyOutgoingCall, ClientCapabilities,
        InitializeParams, InitializedParams, NumberOrString, PartialResultParams,
        SymbolInformation, SymbolKind, TraceValue, Url,
        WindowClientCapabilities, WorkDoneProgressParams, WorkspaceFolder, WorkspaceSymbolParams,
        WorkspaceSymbolResponse,
    },
};
use clap::Parser;
use dialoguer::FuzzySelect;
use fern::colors::{Color, ColoredLevelConfig};
use indexing::{CallHierarchyCache, CallSite, Function, Stop};
use log::{info, warn};
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
    let mut cache: CallHierarchyCache = Default::default();
    
    let colors = ColoredLevelConfig::new()
        .info(Color::Green);
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


    let ((mainloop, mut server), indexed_rx) = indexing::run_server();

    let child = async_process::Command::new(&args.ra_bin)
        .current_dir(&root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(stderr())
        .kill_on_drop(true)
        .spawn()
        .expect("Failed run rust-analyzer");
    let stdout = child.stdout.unwrap();
    let stdin = child.stdin.unwrap();

    let mainloop_fut = tokio::spawn(async move {
        mainloop.run_buffered(stdout, stdin).await.unwrap();
    });

    // Initialize.
    let init_ret = server
        .initialize(InitializeParams {
            workspace_folders: Some(vec![WorkspaceFolder {
                uri: Url::from_file_path(&root).unwrap(),
                name: "root".into(),
            }]),
            capabilities: ClientCapabilities {
                window: Some(WindowClientCapabilities {
                    work_done_progress: Some(true),
                    ..WindowClientCapabilities::default()
                }),
                ..ClientCapabilities::default()
            },
            trace: Some(TraceValue::Verbose),
            work_done_progress_params: WorkDoneProgressParams {
                work_done_token: Some(NumberOrString::String("GGGGG".into())),
            },
            initialization_options: Some(
                serde_json::from_str(
                    r#"{
"files": {"excludeDirs": [".direnv", ".devenv"]},
"workspace": {"symbol": {"search": {"limit": 10000, "kind": "all_symbols", "scope": "workspace"}}},
"cargo": {"targetDir": "target/terrier"}
}"#,
                )
                .unwrap(),
            ),
            ..InitializeParams::default()
        })
        .await
        .unwrap();
    info!("Initialized: {init_ret:?}");
    server.initialized(InitializedParams {}).unwrap();

    // Wait until indexed.
    warn!("Waiting for indexing...");
    indexed_rx.await.unwrap();
    warn!("Indexing done.");

    let mut spinner = Spinner::new(
        spinners::Dots,
        "Indexing functions...",
        spinoff::Color::Blue,
    );

    let mut functions = HashSet::new();
    warn!("Querying for symbols...");
    let ret = server
        .symbol(WorkspaceSymbolParams {
            partial_result_params: PartialResultParams {
                partial_result_token: None,
            },
            work_done_progress_params: WorkDoneProgressParams {
                work_done_token: None,
            },
            query: "".into(),
        })
        .await
        .context("fetching symbols")?;

    if let Some(symbols) = ret {
        match symbols {
            WorkspaceSymbolResponse::Flat(symbols) => {
                for ref s @ SymbolInformation {
                    ref name,
                    ref kind,
                    ref container_name,
                    ..
                } in symbols
                {
                    if matches!(*kind, SymbolKind::FUNCTION) && !args.clutter.contains(name) {
                        functions.insert(Function(s.clone()));
                    }
                    info!(
                        "{:?} {}::{}",
                        kind,
                        container_name.clone().unwrap_or_default(),
                        name
                    );
                }
            }
            WorkspaceSymbolResponse::Nested(workspace_symbols) => todo!(),
        }
    } else {
        info!("None.");
    }

    let mut functions = functions.into_iter().collect::<Vec<_>>();
    info!("{} functions found", functions.len());
    functions.sort_by(|f1, f2| {
        f1.0.location.uri.cmp(&f2.0.location.uri).then(
            f1.0.location
                .range
                .start
                .line
                .cmp(&f2.0.location.range.start.line),
        )
    });
    let function_names = functions
        .iter()
        .map(|f| f.pretty(&root))
        .collect::<Vec<_>>();

    spinner.stop_and_persist("", "Functions indexed");

    while let Some(selection) = FuzzySelect::new()
        .items(&function_names)
        .max_length(15)
        .interact_opt()?
    {
        let mut spinner = Spinner::new(spinners::Dots, "Generating...", spinoff::Color::Blue);
        let f = &functions[selection];

        let (incomings, outgoings) = {
            if let Some(callsites) = cache.get(f) {
                callsites
            } else {
                let mut incomings = Vec::new();
                let mut outgoings = Vec::new();

                if let Some(xs) = server
                    .incoming_calls(f.into())
                    .await
                    .context("failed to fetch incomings")?
                {
                    let mut incomings_set = HashSet::new();
                    for CallHierarchyIncomingCall {
                        from: ff @ CallHierarchyItem { name, .. },
                        ..
                    } in xs.iter()
                    {
                        if !args.clutter.contains(&name) {
                            incomings_set.insert(CallSite(ff.clone()));
                        }
                    }
                    incomings.extend(incomings_set.into_iter());
                }

                if let Some(xs) = server
                    .outgoing_calls(f.into())
                    .await
                    .context("failed to fetch outgoings")?
                {
                    let mut outgoings_set = HashSet::new();
                    for CallHierarchyOutgoingCall {
                        to: ff @ CallHierarchyItem { name, .. },
                        ..
                    } in xs.iter()
                    {
                        if !args.clutter.contains(&name) {
                            outgoings_set.insert(CallSite(ff.clone()));
                        }
                    }
                    outgoings.extend(outgoings_set.into_iter());
                }

                incomings.sort_by(|f1, f2| {
                    f1.0.uri
                        .cmp(&f2.0.uri)
                        .then(f1.0.range.start.line.cmp(&f2.0.range.start.line))
                });
                outgoings.sort_by(|f1, f2| {
                    f1.0.uri
                        .cmp(&f2.0.uri)
                        .then(f1.0.range.start.line.cmp(&f2.0.range.start.line))
                });

                cache.insert(f.to_owned(), (incomings, outgoings));
                cache.get(f).unwrap()
            }
        };
        spinner.clear();

        let header = [
            "".to_string(),
            "".to_string(),
            f.pretty(&root),
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

    // Shutdown.
    server.shutdown(()).await.unwrap();
    server.exit(()).unwrap();

    server.emit(Stop).unwrap();
    mainloop_fut.await.unwrap();

    Ok(())
}
