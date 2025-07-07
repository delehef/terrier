use anyhow::Context;
use colored::Colorize;
use dialoguer::FuzzySelect;
use notify_rust::Notification;
use spinoff::{Spinner, spinners};
use std::{
    io::Write,
    path::{Path, PathBuf},
};

use crate::indexing::Index;

fn menu(tty: &mut console::Term, title: &str, choices: &[(char, impl AsRef<str>)]) -> char {
    let prompt = format!(
        "{} {}",
        title.white().bold(),
        choices
            .iter()
            .map(|(trigger, rest)| format!(
                "{}{}",
                format!("[{trigger}]").yellow().bold(),
                rest.as_ref()
            ),)
            .collect::<Vec<_>>()
            .join(" - "),
    );
    writeln!(tty, "{prompt}").unwrap();
    loop {
        match tty.read_char().unwrap() {
            x if choices.iter().any(|(trigger, _)| *trigger == x) => return x,
            _ => {}
        }
    }
}

pub struct Settings {
    only_own: bool,
}
impl Default for Settings {
    fn default() -> Self {
        Self { only_own: true }
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
        let mut explore_stack = explore_stack.clone();
        loop {
            let f_id = if let Some(i) = explore_stack.last() {
                *i
            } else {
                if let Some(i) = FuzzySelect::new()
                    .with_prompt("Select a function - <ESC> quit")
                    .items(&self.function_names)
                    .max_length(15)
                    .interact_opt()?
                {
                    i
                } else {
                    return Ok(());
                }
            };

            let start = std::time::Instant::now();
            let mut spinner = Spinner::new(spinners::Dots, "Generating...", spinoff::Color::Blue);
            let (incomings, outgoings) = self.indexer.context(f_id).await?;
            spinner.clear();
            let f = &self.indexer.functions[f_id];
            if start.elapsed().as_secs() > 10 {
                Notification::new()
                    .summary("Function ready")
                    .body(&format!("{} has been successfully racked", f.0.name))
                    .appname("Terrier")
                    .show()?;
            }

            let choices = incomings
                .iter()
                .filter(|f| {
                    self.indexer.fn_id_from_callsite(f).is_some()
                        && if self.settings.only_own {
                            Path::new(f.0.uri.path()).starts_with(&self.root)
                        } else {
                            true
                        }
                })
                .map(|f| f.0.name.bright_blue().bold().to_string())
                .chain(
                    outgoings
                        .iter()
                        .filter(|f| {
                            self.indexer.fn_id_from_callsite(f).is_some()
                                && if self.settings.only_own {
                                    Path::new(f.0.uri.path()).starts_with(&self.root)
                                } else {
                                    true
                                }
                        })
                        .map(|f| f.0.name.bright_purple().bold().to_string()),
                )
                .enumerate()
                .map(|(i, f)| (i.to_string().chars().next().unwrap(), f))
                .take(10)
                .chain(vec![('b', "ack".into()), ('j', "ump".into())])
                .collect::<Vec<_>>();

            let i_to_fn_id = incomings
                .iter()
                .chain(outgoings.iter())
                .filter(|f| {
                    self.indexer.fn_id_from_callsite(f).is_some()
                        && if self.settings.only_own {
                            Path::new(f.0.uri.path()).starts_with(&self.root)
                        } else {
                            true
                        }
                })
                .filter_map(|f| self.indexer.fn_id_from_callsite(f))
                .take(10)
                .collect::<Vec<_>>();

            let (left_column, left_pad) = std::iter::once("CALLERS".blue())
                .chain(
                    incomings
                        .iter()
                        .filter(|i| {
                            if self.settings.only_own {
                                Path::new(i.0.uri.path()).starts_with(&self.root)
                            } else {
                                true
                            }
                        })
                        .map(|f| f.0.name.bright_blue().bold()),
                )
                .fold((Vec::new(), 10), |(mut cells, pad), f| {
                    let pad = pad.max(f.len() + 3);
                    cells.push(f);
                    (cells, pad)
                });

            let (center_column, center_pad) = (
                vec!["CURRENT".white(), f.0.name.bright_white()],
                f.0.name.len() + 3,
            );

            let (right_column, right_pad) = std::iter::once("CALLEES".purple())
                .chain(
                    outgoings
                        .iter()
                        .filter(|i| {
                            if self.settings.only_own {
                                Path::new(i.0.uri.path()).starts_with(&self.root)
                            } else {
                                true
                            }
                        })
                        .map(|f| f.0.name.bright_purple().bold()),
                )
                .fold((Vec::new(), 10), |(mut cells, pad), f| {
                    let pad = pad.max(f.len() + 3);
                    cells.push(f);
                    (cells, pad)
                });

            println!("\n\n");
            for i in 0..left_column
                .len()
                .max(center_column.len())
                .max(right_column.len())
            {
                println!(
                    "{:left_pad$} {:center_pad$} {:right_pad$}",
                    left_column.get(i).unwrap_or(&"".white()),
                    center_column.get(i).unwrap_or(&"".white()),
                    right_column.get(i).unwrap_or(&"".white())
                );
            }

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

            match menu(&mut self.tty, "Goto...", &choices) {
                i @ ('0'..'9') => {
                    let i = i.to_digit(10).unwrap() as usize;
                    let new_f_id = i_to_fn_id[i];

                    // Only push the new frame if we are not already in it
                    if explore_stack
                        .last()
                        .map(|top| *top != new_f_id)
                        .unwrap_or(true)
                    {
                        explore_stack.push(new_f_id);
                    }
                }
                'b' => {
                    if explore_stack.is_empty() {
                        return Ok(());
                    } else {
                        explore_stack.pop();
                    }
                }
                'j' => {
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
                _ => unreachable!(),
            }
        }
    }
}
