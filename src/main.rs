use std::{
    collections::HashSet, default, fmt::Display, io::stderr, ops::ControlFlow, path::Path,
    process::Stdio,
};

use anyhow::{Context, ensure};
use async_lsp::{
    Error, ErrorCode, LanguageServer,
    concurrency::ConcurrencyLayer,
    lsp_types::{
        CallHierarchyIncomingCall, CallHierarchyIncomingCallsParams, CallHierarchyItem,
        CallHierarchyOutgoingCall, CallHierarchyOutgoingCallsParams, ClientCapabilities,
        DidOpenTextDocumentParams, InitializeParams, InitializedParams, NumberOrString,
        PartialResultParams, ProgressParamsValue, SymbolInformation, SymbolKind, TextDocumentItem,
        TraceValue, Url, WindowClientCapabilities, WorkDoneProgress, WorkDoneProgressBegin,
        WorkDoneProgressEnd, WorkDoneProgressParams, WorkDoneProgressReport, WorkspaceFolder,
        WorkspaceSymbolParams, WorkspaceSymbolResponse,
        notification::{Progress, PublishDiagnostics, ShowMessage},
    },
    panic::CatchUnwindLayer,
    router::Router,
    tracing::TracingLayer,
};
use clap::Parser;
use fern::colors::{Color, ColoredLevelConfig};
use log::{error, info, warn};
use tower::ServiceBuilder;

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

struct ClientState {
    indexed_tx: Option<oneshot::Sender<()>>,
}

struct Stop;

#[derive(PartialEq)]
struct CallSite(CallHierarchyItem);
impl CallSite {
    fn pretty<P: AsRef<Path>>(&self, root: P) -> String {
        let dets = if let Some(d) = self.0.detail.as_ref() {
            match syn::parse_str::<syn::Signature>(d) {
                Ok(sig) => {
                    let outputs = match sig.output {
                        syn::ReturnType::Default => String::new(),
                        syn::ReturnType::Type(_, t) => format!("{t:?}"),
                    };
                    format!("{} -> {}", sig.ident, outputs)
                }
                Err(err) => format!("{d} -- {err:?}"),
            }
        } else {
            "N/A".into()
        };
        let root = root.as_ref();
        let relative_file = self
            .0
            .uri
            .path()
            .strip_prefix(root.as_os_str().to_str().unwrap())
            .unwrap();
        format!(
            "{relative_file}:{},{} {}",
            self.0.range.start.line, self.0.range.start.character, self.0.name
        )
    }
}
impl std::hash::Hash for CallSite {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.0.uri.hash(state);
        self.0.range.hash(state);
    }
}
impl Eq for CallSite {}

#[derive(PartialEq, Eq)]
struct Function(SymbolInformation);
impl Function {
    fn pretty<P: AsRef<Path>>(&self, root: P) -> String {
        let root = root.as_ref();
        let relative_file = self
            .0
            .location
            .uri
            .path()
            .strip_prefix(root.as_os_str().to_str().unwrap())
            .unwrap();
        format!(
            "{}:{} {}{}",
            relative_file,
            self.0.location.range.start.line,
            self.0
                .container_name
                .as_ref()
                .map(|c| format!("{c}::"))
                .unwrap_or_default(),
            self.0.name
        )
    }
}
impl std::hash::Hash for Function {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.0.location.uri.hash(state);
        self.0.location.range.hash(state);
    }
}
impl From<&Function> for CallHierarchyItem {
    fn from(f: &Function) -> Self {
        CallHierarchyItem {
            name: f.0.name.clone(),
            kind: f.0.kind.clone(),
            tags: f.0.tags.clone(),
            detail: None,
            uri: f.0.location.uri.clone(),
            range: f.0.location.range.clone(),
            selection_range: f.0.location.range.clone(),
            data: None,
        }
    }
}
impl From<&Function> for CallHierarchyIncomingCallsParams {
    fn from(f: &Function) -> Self {
        CallHierarchyIncomingCallsParams {
            item: f.into(),
            work_done_progress_params: WorkDoneProgressParams {
                work_done_token: None,
            },
            partial_result_params: PartialResultParams {
                partial_result_token: None,
            },
        }
    }
}
impl From<&Function> for CallHierarchyOutgoingCallsParams {
    fn from(f: &Function) -> Self {
        CallHierarchyOutgoingCallsParams {
            item: f.into(),
            work_done_progress_params: WorkDoneProgressParams {
                work_done_token: None,
            },
            partial_result_params: PartialResultParams {
                partial_result_token: None,
            },
        }
    }
}

const RA_INDEXING_TOKENS: &[&str] = &["rustAnalyzer/Indexing", "rustAnalyzer/cachePriming"];

fn asdf() -> i32 {
    4
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    asdf();
    let colors = ColoredLevelConfig::new()
        // use builder methods
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

    let (indexed_tx, indexed_rx) = oneshot::channel();

    let (mainloop, mut server) = async_lsp::MainLoop::new_client(|_server| {
        let mut router = Router::new(ClientState {
            indexed_tx: Some(indexed_tx),
        });
        router
            .notification::<Progress>(|this, prog| {
                let token = match &prog.token{
                    NumberOrString::Number(x) => x.to_string(),
                    NumberOrString::String(s) => s.strip_prefix("rustAnalyzer/").to_owned().unwrap_or_default().to_string(),
                };

                if let ProgressParamsValue::WorkDone(ref progress) = prog.value {
                    match progress {
                        WorkDoneProgress::Begin(WorkDoneProgressBegin{title, message, percentage, ..})=> info!("[{}{}] {} {}", token, title, percentage.map(|x| format!(" {x}%")).unwrap_or_default(), message.as_ref().cloned().unwrap_or(String::new())),
                        WorkDoneProgress::Report(WorkDoneProgressReport{message, percentage, ..}) => info!("[{}{}] {}", token, percentage.map(|x| format!(" {x}%")).unwrap_or_default(), message.as_ref().cloned().unwrap_or(String::new())),
                        WorkDoneProgress::End(WorkDoneProgressEnd{message})=> info!("{} {}", token, message.as_ref().cloned().unwrap_or("done".to_owned()))
                    }
                } else {
                }
                if matches!(prog.token, NumberOrString::String(s) if RA_INDEXING_TOKENS.contains(&&*s))
                    && matches!(
                        prog.value,
                        ProgressParamsValue::WorkDone(WorkDoneProgress::End(_))
                    )
                {
                    // Sometimes rust-analyzer auto-index multiple times?
                    if let Some(tx) = this.indexed_tx.take() {
                        let _: Result<_, _> = tx.send(());
                    }
                }
                ControlFlow::Continue(())
            })
            .notification::<PublishDiagnostics>(|_this, diag| { info!("DIAG {:?}", diag); ControlFlow::Continue(())})
            .notification::<ShowMessage>(|_, params| {
                info!("Message {:?}: {}", params.typ, params.message);
                ControlFlow::Continue(())
            })
            .event(|_, _: Stop| ControlFlow::Break(Ok(())));

        ServiceBuilder::new()
            .layer(TracingLayer::default())
            .layer(CatchUnwindLayer::default())
            .layer(ConcurrencyLayer::default())
            .service(router)
    });

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

    for (i, f) in functions.iter().enumerate() {
        let mut incomings = HashSet::new();
        let mut outgoings = HashSet::new();

        if let Some(xs) = server
            .incoming_calls(f.into())
            .await
            .context("failed to fetch incomings")?
        {
            for CallHierarchyIncomingCall {
                from: ff @ CallHierarchyItem { name, .. },
                ..
            } in xs.iter()
            {
                if !args.clutter.contains(name) {
                    incomings.insert(CallSite(ff.clone()));
                }
            }
        }

        if let Some(xs) = server
            .outgoing_calls(f.into())
            .await
            .context("failed to fetch outgoings")?
        {
            for CallHierarchyOutgoingCall {
                to: ff @ CallHierarchyItem { name, .. },
                ..
            } in xs.iter()
            {
                if !args.clutter.contains(name) {
                    outgoings.insert(CallSite(ff.clone()));
                }
            }
        }

        println!("\n\n{}", f.pretty(&root));
        for i in incomings
            .into_iter()
            .filter(|i| Path::new(i.0.uri.path()).starts_with(&root))
        {
            println!("    <-- {}", i.pretty(&root));
        }
        for o in outgoings
            .into_iter()
            .filter(|o| Path::new(o.0.uri.path()).starts_with(&root))
        {
            println!("    --> {}", o.pretty(&root));
        }
    }

    // Shutdown.
    server.shutdown(()).await.unwrap();
    server.exit(()).unwrap();

    server.emit(Stop).unwrap();
    mainloop_fut.await.unwrap();

    Ok(())
}
