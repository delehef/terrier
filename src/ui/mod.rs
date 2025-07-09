use anyhow::Context;
use colored::Colorize;
use compact_str::CompactString;
use debruijn::DeBruijner;
use dialoguer::FuzzySelect;
#[cfg(target_os = "linux")]
use notify_rust::Notification;
use prompt::{Entry, menu};
use spinoff::{Spinner, spinners};
use std::path::PathBuf;

use crate::indexing::{FunctionId, Index};

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
        self.function_loop(Vec::new()).await?;
        self.indexer
            .shutdown()
            .await
            .context("shutting down indexer")?;

        Ok(())
    }

    pub async fn function_loop(&mut self, explore_stack: Vec<usize>) -> anyhow::Result<()> {
        #[derive(Clone)]
        enum FnAction {
            GoTo(FunctionId),
            Jump,
            Back,
            Quit,
            OpenIn,
        }
        let mut explore_stack = explore_stack.clone();
        loop {
            let f_id: FunctionId = if let Some(i) = explore_stack.last() {
                *i
            } else if let Some(i) = FuzzySelect::new()
                .with_prompt("Select a function - <ESC> quit")
                .items(&self.function_names)
                .max_length(15)
                .interact_opt()?
            {
                explore_stack.push(i);
                i
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
                .chain(incomings.iter().enumerate().map(|(i, f)| {
                    format!(
                        "[{}] {}",
                        chords[i].yellow().bold(),
                        f.0.name.bright_blue().bold()
                    )
                }))
                .collect::<Vec<_>>();

            let center_column = vec![
                "CURRENT".white().to_string(),
                f.0.name.bright_white().to_string(),
            ];

            let right_column = std::iter::once("CALLEES".purple().to_string())
                .chain(outgoings.iter().enumerate().map(|(i, f)| {
                    format!(
                        "[{}] {}",
                        chords[i + incomings.len()].yellow().bold(),
                        f.0.name.bright_purple().bold()
                    )
                }))
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

            if !explore_stack.is_empty() {
                println!(
                    "\nExploration Stack\n{}\n",
                    explore_stack
                        .iter()
                        .map(|f_id| &self.indexer.functions[*f_id])
                        .map(|f| format!(
                            "{:50} {}",
                            format!(
                                "{}:{}",
                                f.0.location
                                    .uri
                                    .path()
                                    .strip_prefix(self.root.as_os_str().to_str().unwrap())
                                    .unwrap_or(f.0.location.uri.path())
                                    .bright_black(),
                                f.0.location.range.start.line,
                            ),
                            f.0.name.bright_white().bold()
                        ))
                        .collect::<Vec<_>>()
                        .join("\n")
                );
            }

            match menu(&mut self.tty, "", choices)? {
                FnAction::GoTo(new_fn_id) => {
                    // Only push the new frame if we are not already in it
                    if explore_stack
                        .last()
                        .map(|top| *top != *new_fn_id)
                        .unwrap_or(true)
                    {
                        explore_stack.push(*new_fn_id);
                    }
                }
                FnAction::Quit => return Ok(()),
                FnAction::Back => {
                    if explore_stack.is_empty() {
                        return Ok(());
                    } else {
                        explore_stack.pop();
                    }
                }
                FnAction::Jump => {
                    let new_f_id = FuzzySelect::new()
                        .items(&self.function_names)
                        .max_length(15)
                        .interact()?;

                    // Only push the new frame if we are not already in it
                    if explore_stack
                        .last()
                        .map(|top| *top != new_f_id)
                        .unwrap_or(true)
                    {
                        explore_stack.push(new_f_id);
                    }
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
