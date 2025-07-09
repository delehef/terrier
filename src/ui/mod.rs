use anyhow::Context;
use colored::{Color, Colorize};
use compact_str::CompactString;
use debruijn::DeBruijner;
use dialoguer::FuzzySelect;
#[cfg(target_os = "linux")]
use notify_rust::Notification;
use prompt::{Entry, menu};
use spinoff::{Spinner, spinners};
use std::path::{Path, PathBuf};
use tabled::Table;

use crate::indexing::{Function, FunctionId, Index};

mod debruijn;
mod prompt;

pub struct Settings {
    only_in_project: bool,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            only_in_project: true,
        }
    }
}

pub struct Ui {
    root: PathBuf,
    tty: console::Term,
    indexer: Index,
    function_names: Vec<String>,
    settings: Settings,
}

impl Ui {
    pub fn new(indexer: Index) -> anyhow::Result<Self> {
        let function_names = indexer
            .functions
            .iter()
            .map(|f| f.pretty(&indexer.root))
            .collect::<Vec<_>>();

        Ok(Self {
            root: indexer.root.clone(),
            tty: console::Term::stdout(),
            indexer,
            function_names,
            settings: Settings::default(),
        })
    }

    pub async fn run(mut self) -> anyhow::Result<()> {
        self.function_loop(NavigationStack::default()).await?;
        self.indexer
            .shutdown()
            .await
            .context("shutting down indexer")?;

        Ok(())
    }

    pub async fn function_loop(
        &mut self,
        mut navigation_stack: NavigationStack,
    ) -> anyhow::Result<()> {
        #[derive(Clone)]
        enum FnAction {
            GoTo(FunctionId),
            Jump,
            Back,
            Quit,
            OpenIn,
        }
        loop {
            let f_id: FunctionId = if let Some(i) = navigation_stack.current() {
                *i
            } else if let Some(i) = FuzzySelect::new()
                .with_prompt("Select a function - <ESC> quit")
                .items(&self.function_names)
                .max_length(15)
                .interact_opt()?
            {
                let fn_id: FunctionId = i.into();
                navigation_stack.push(fn_id);
                fn_id
            } else {
                return Ok(());
            }
            .into();

            #[cfg(target_os = "linux")]
            let start = std::time::Instant::now();

            let mut spinner = Spinner::new(spinners::Dots, "Generating...", spinoff::Color::Blue);
            let (incomings, outgoings) = self
                .indexer
                .context(f_id, self.settings.only_in_project)
                .await?;
            spinner.clear();
            let f = &self.indexer.functions[*f_id];

            #[cfg(target_os = "linux")]
            if start.elapsed().as_secs() > 10 {
                Notification::new()
                    .summary("Function ready")
                    .body(&format!("{} has been successfully tracked", f.0.name))
                    .finalize()
                    .show()?;
            }

            let chords = DeBruijner::default().generate_n(incomings.len() + outgoings.len());

            let choices = incomings
                .iter()
                .chain(outgoings.iter())
                .filter_map(|f| self.indexer.fn_id_from_callsite(f).map(|id| (f, id)))
                .enumerate()
                .map(|(i, (f, f_id))| Entry {
                    chord: chords[i].clone(),
                    label: CompactString::from(&f.0.name),
                    payload: FnAction::GoTo(f_id),
                    show: false,
                })
                .chain(vec![
                    Entry {
                        chord: "o".into(),
                        label: "open...".into(),
                        payload: FnAction::OpenIn,
                        show: true,
                    },
                    Entry {
                        chord: "b".into(),
                        label: "back".into(),
                        payload: FnAction::Back,
                        show: true,
                    },
                    Entry {
                        chord: "j".into(),
                        label: "jump".into(),
                        payload: FnAction::Jump,
                        show: true,
                    },
                    Entry {
                        chord: "q".into(),
                        label: "quit".into(),
                        payload: FnAction::Quit,
                        show: true,
                    },
                ])
                .collect::<Vec<_>>();

            let left_column = std::iter::once("CALLERS".blue().to_string())
                .chain(
                    incomings
                        .iter()
                        .filter(|f| self.indexer.fn_id_from_callsite(f).is_some())
                        .enumerate()
                        .map(|(i, f)| {
                            format!(
                                "[{}] {}",
                                chords[i].yellow().bold(),
                                f.0.name.bright_blue().bold()
                            )
                        }),
                )
                .collect::<Vec<_>>();

            let center_column = vec![
                "CURRENT".white().to_string(),
                f.0.name.bright_white().to_string(),
            ];

            let right_column = std::iter::once("CALLEES".purple().to_string())
                .chain(
                    outgoings
                        .iter()
                        .filter(|f| self.indexer.fn_id_from_callsite(f).is_some())
                        .enumerate()
                        .map(|(i, f)| {
                            format!(
                                "[{}] {}",
                                chords[i + incomings.len()].yellow().bold(),
                                f.0.name.bright_purple().bold()
                            )
                        }),
                )
                .collect::<Vec<_>>();

            let mut tabled = tabled::builder::Builder::new();
            tabled.push_column(left_column);
            tabled.push_column(center_column);
            tabled.push_column(right_column);
            let mut table = tabled.build();
            table.with(
                tabled::settings::Style::sharp()
                    .remove_verticals()
                    .remove_frame(),
            );
            println!("{table}");

            if let Some(table) =
                navigation_stack.to_table(&self.root, |f_id| &self.indexer.functions[*f_id])
            {
                println!("\n\n{table}");
            }

            match menu(&mut self.tty, "", choices)? {
                FnAction::GoTo(new_fn_id) => {
                    navigation_stack.push(new_fn_id);
                }
                FnAction::Quit => return Ok(()),
                FnAction::Back => {
                    if navigation_stack.is_empty() {
                        return Ok(());
                    } else {
                        navigation_stack.pop();
                    }
                }
                FnAction::Jump => {
                    let new_f_id = FuzzySelect::new()
                        .items(&self.function_names)
                        .max_length(15)
                        .interact()?;

                    navigation_stack.push(new_f_id.into());
                }
                FnAction::OpenIn => {
                    let _ = std::process::Command::new("emacsclient")
                        // LSP lines start at 1
                        .arg(format!("+{}", f.0.location.range.start.line + 1))
                        .arg(format!("{}", f.0.location.uri.path()))
                        .spawn();
                }
            }
        }
    }
}

#[derive(Default, Clone)]
struct NavigationStack(Vec<FunctionId>);
impl NavigationStack {
    fn current(&self) -> Option<&FunctionId> {
        self.0.last()
    }

    fn push(&mut self, f: FunctionId) {
        // Only push the new frame if we are not already in it
        if self.0.last().map(|top| *top != f).unwrap_or(true) {
            self.0.push(f);
        }
    }

    fn pop(&mut self) -> Option<FunctionId> {
        self.0.pop()
    }

    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn to_table<'a, P: AsRef<Path>, F: Fn(FunctionId) -> &'a Function>(
        &self,
        root: P,
        f: F,
    ) -> Option<Table> {
        if self.is_empty() {
            return None;
        };

        let mut table = Table::nohead(self.0.iter().map(|f_id| f(*f_id)).enumerate().map(
            |(i, f)| {
                let step = 80 + (((255 - 80) * i) / self.0.len()) as u8;
                (
                    f.0.name
                        .color(Color::TrueColor {
                            r: step,
                            g: step,
                            b: step,
                        })
                        .to_string(),
                    format!(
                        "{}:{}",
                        f.0.location
                            .uri
                            .path()
                            .strip_prefix(root.as_ref().as_os_str().to_str().unwrap())
                            .unwrap_or(f.0.location.uri.path()),
                        f.0.location.range.start.line,
                    )
                    .bright_black()
                    .to_string(),
                )
            },
        ));
        table.with(tabled::settings::Style::blank());
        table.with(tabled::settings::Panel::header(
            "Navigation Stack".bright_white().to_string(),
        ));

        Some(table)
    }
}
