use std::{
    collections::{HashMap, HashSet},
    ops::ControlFlow,
    path::{Path, PathBuf},
    process::Stdio,
};

use anyhow::Context;
use async_lsp::{
    LanguageServer, ServerSocket,
    concurrency::ConcurrencyLayer,
    lsp_types::{
        CallHierarchyIncomingCall, CallHierarchyIncomingCallsParams, CallHierarchyItem,
        CallHierarchyOutgoingCall, CallHierarchyOutgoingCallsParams, ClientCapabilities,
        InitializeParams, InitializedParams, NumberOrString, PartialResultParams,
        ProgressParamsValue, SymbolInformation, SymbolKind, TraceValue, Url,
        WindowClientCapabilities, WorkDoneProgress, WorkDoneProgressBegin, WorkDoneProgressEnd,
        WorkDoneProgressParams, WorkDoneProgressReport, WorkspaceFolder, WorkspaceSymbolParams,
        WorkspaceSymbolResponse,
        notification::{Progress, PublishDiagnostics, ShowMessage},
    },
    panic::CatchUnwindLayer,
    router::Router,
    tracing::TracingLayer,
};
use async_process::Child;
use colored::Colorize;
use log::info;
use tokio::task::JoinHandle;
use tower::ServiceBuilder;

pub struct ClientState {
    indexed_tx: Option<oneshot::Sender<()>>,
}

pub struct Stop;

pub type CallHierarchyCache = HashMap<Function, (Vec<CallSite>, Vec<CallSite>)>;

#[derive(Clone, PartialEq)]
pub struct CallSite(pub CallHierarchyItem);
impl CallSite {
    pub fn pretty<P: AsRef<Path>>(&self, root: P) -> String {
        let _dets = if let Some(d) = self.0.detail.as_ref() {
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
            "{}:{} {}",
            relative_file.bright_black(),
            format!(
                "({}, {})",
                self.0.range.start.line, self.0.range.start.character,
            )
            .red(),
            self.0.name.bold().bright_yellow()
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

#[derive(Clone, PartialEq, Eq)]
pub struct Function(pub SymbolInformation);
impl Function {
    pub fn pretty<P: AsRef<Path>>(&self, root: P) -> String {
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
            relative_file.bright_black(),
            self.0.location.range.start.line,
            self.0
                .container_name
                .as_ref()
                .map(|c| format!("{c}::"))
                .unwrap_or_default()
                .yellow(),
            self.0.name.bold().bright_yellow()
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
            kind: f.0.kind,
            tags: f.0.tags.clone(),
            detail: None,
            uri: f.0.location.uri.clone(),
            range: f.0.location.range,
            selection_range: f.0.location.range,
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

pub struct Index {
    /// A handle to the LS interaction loop
    mainloop_fut: JoinHandle<()>,
    /// A socket to talk to the LS.
    server: ServerSocket,
    /// A handle to the LS process.
    _child: Child,

    pub root: PathBuf,
    clutter: HashSet<String>,
    pub functions: Vec<Function>,
    cache: CallHierarchyCache,
}
impl Index {
    pub async fn new<P: AsRef<Path>>(
        root: P,
        clutter: HashSet<String>,
        ra_bin: &str,
    ) -> anyhow::Result<Self> {
        let (indexed_tx, indexed_rx) = oneshot::channel();

        let ((mainloop, mut server), indexed_rx) = (
            async_lsp::MainLoop::new_client(|_server| {
                let mut router = Router::new(ClientState {
                    indexed_tx: Some(indexed_tx),
                });
                router
            .notification::<Progress>(|this, prog| {
                let token = match &prog.token{
                    NumberOrString::Number(x) => x.to_string(),
                    NumberOrString::String(s) => s.strip_prefix("rustAnalyzer/").to_owned().unwrap_or_default().to_string(),
                };

                let ProgressParamsValue::WorkDone(ref progress) = prog.value;
                    match progress {
                        WorkDoneProgress::Begin(WorkDoneProgressBegin{title, message, percentage, ..})=> info!("[{}{}] {} {}", token, title, percentage.map(|x| format!(" {x}%")).unwrap_or_default(), message.as_ref().cloned().unwrap_or(String::new())),
                        WorkDoneProgress::Report(WorkDoneProgressReport{message, percentage, ..}) => info!("[{}{}] {}", token, percentage.map(|x| format!(" {x}%")).unwrap_or_default(), message.as_ref().cloned().unwrap_or(String::new())),
                        WorkDoneProgress::End(WorkDoneProgressEnd{message})=> info!("{} {}", token, message.as_ref().cloned().unwrap_or("done".to_owned()))
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
            }),
            indexed_rx,
        );

        let mut _child = async_process::Command::new(ra_bin)
            .current_dir(&root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(std::io::stderr())
            .kill_on_drop(true)
            .spawn()
            .expect("Failed run rust-analyzer");
        let stdout = _child.stdout.take().unwrap();
        let stdin = _child.stdin.take().unwrap();

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

        indexed_rx.await.unwrap();
        info!("Project indexed.");

        let mut functions = HashSet::new();
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
                        if matches!(*kind, SymbolKind::FUNCTION) && !clutter.contains(name) {
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
                WorkspaceSymbolResponse::Nested(_workspace_symbols) => todo!(),
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

        Ok(Self {
            root: root.as_ref().to_path_buf(),
            // Keep a handle to the LS process so that it does not die
            _child,
            clutter,
            functions,
            cache: Default::default(),
            mainloop_fut,
            server,
        })
    }

    pub async fn context(&mut self, f_id: usize) -> anyhow::Result<(Vec<CallSite>, Vec<CallSite>)> {
        let (incomings, outgoings) = {
            if let Some(callsites) = self.cache.get(&self.functions[f_id]) {
                callsites
            } else {
                let mut incomings = Vec::new();
                let mut outgoings = Vec::new();

                if let Some(xs) = self
                    .server
                    .incoming_calls((&self.functions[f_id]).into())
                    .await
                    .context("failed to fetch incomings")?
                {
                    let mut incomings_set = HashSet::new();
                    for CallHierarchyIncomingCall {
                        from: ff @ CallHierarchyItem { name, .. },
                        ..
                    } in xs.iter()
                    {
                        if !self.clutter.contains(name) {
                            incomings_set.insert(CallSite(ff.clone()));
                        }
                    }
                    incomings.extend(incomings_set.into_iter());
                }

                if let Some(xs) = self
                    .server
                    .outgoing_calls((&self.functions[f_id]).into())
                    .await
                    .context("failed to fetch outgoings")?
                {
                    let mut outgoings_set = HashSet::new();
                    for CallHierarchyOutgoingCall {
                        to: ff @ CallHierarchyItem { name, .. },
                        ..
                    } in xs.iter()
                    {
                        if !self.clutter.contains(name) {
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

                let f = self.functions[f_id].clone();
                self.cache.insert(f.clone(), (incomings, outgoings));
                self.cache.get(&f).unwrap()
            }
        };

        Ok((incomings.to_vec(), outgoings.to_vec()))
    }

    pub async fn shutdown(mut self) -> anyhow::Result<()> {
        self.server.shutdown(()).await.unwrap();
        self.server.exit(()).unwrap();

        self.server.emit(Stop).unwrap();
        self.mainloop_fut.await?;
        Ok(())
    }
}
