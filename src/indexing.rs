use std::{collections::HashMap, ops::ControlFlow, path::Path};

use async_lsp::{
    concurrency::{Concurrency, ConcurrencyLayer},
    lsp_types::{
        notification::{Progress, PublishDiagnostics, ShowMessage},
        CallHierarchyIncomingCallsParams, CallHierarchyItem, CallHierarchyOutgoingCallsParams,
        NumberOrString, PartialResultParams, ProgressParamsValue, SymbolInformation,
        WorkDoneProgress, WorkDoneProgressBegin, WorkDoneProgressEnd, WorkDoneProgressParams,
        WorkDoneProgressReport,
    },
    panic::{CatchUnwind, CatchUnwindLayer},
    router::Router,
    tracing::{Tracing, TracingLayer},
    MainLoop, ServerSocket,
};
use colored::Colorize;
use log::info;
use tower::ServiceBuilder;

pub struct ClientState {
    indexed_tx: Option<oneshot::Sender<()>>,
}

pub struct Stop;

pub type CallHierarchyCache = HashMap<Function, (Vec<CallSite>, Vec<CallSite>)>;

#[derive(PartialEq)]
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

pub fn run_server() -> (
    (
        MainLoop<Tracing<CatchUnwind<Concurrency<Router<ClientState>>>>>,
        ServerSocket,
    ),
    oneshot::Receiver<()>,
) {
    let (indexed_tx, indexed_rx) = oneshot::channel();
    (
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
    )
}
