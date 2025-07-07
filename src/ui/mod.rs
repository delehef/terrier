use anyhow::Context;
use colored::Colorize;
use dialoguer::FuzzySelect;
use spinoff::{Spinner, spinners};
use std::{
    io::Write,
    path::{Path, PathBuf},
};
use tabled::tables::IterTable;

use crate::indexing::{CallSite, Index};

fn menu<S: AsRef<str>>(
    tty: &mut console::Term,
    title: &str,
    choices: &[(char, S)],
) -> Option<char> {
    let prompt = format!(
        "{}: {} - {}uit",
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
        "[q]".red().bold(),
    );
    loop {
        writeln!(tty, "{prompt}").unwrap();
        match tty.read_char().unwrap() {
            x if choices.iter().any(|(trigger, _)| *trigger == x) => return Some(x),
            'q' => return None,
            _ => {}
        }
    }
}

pub struct Ui {
    root: PathBuf,
    tty: console::Term,
    indexer: Index,
    function_names: Vec<String>,
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
        })
    }

    pub async fn run(mut self) -> anyhow::Result<()> {
        self.tty.write_line("")?;

        while let Some(choice) = menu(&mut self.tty, "", &[('f', "unction")]) {
            match choice {
                'f' => self.jump_to_function().await?,
                'q' => break,
                _ => unreachable!(),
            }
        }

        self.indexer
            .shutdown()
            .await
            .context("shutting down indexer")?;

        Ok(())
    }

    pub async fn jump_to_function(&mut self) -> anyhow::Result<()> {
        if let Some(f_id) = FuzzySelect::new()
            .items(&self.function_names)
            .max_length(15)
            .interact_opt()?
        {
            self.show_function(f_id, Vec::new()).await
        } else {
            Ok(())
        }
    }

    pub async fn show_function(
        &mut self,
        f_id: usize,
        mut explore_stack: Vec<usize>,
    ) -> anyhow::Result<()> {
        let mut spinner = Spinner::new(spinners::Dots, "Generating...", spinoff::Color::Blue);
        let (incomings, outgoings) = self.indexer.context(f_id).await?;
        spinner.clear();

        let choices = incomings
            .iter()
            .chain(outgoings.iter())
            .filter(|f| self.indexer.fn_id_from_callsite(f).is_some())
            .map(|f| f.0.name.bright_purple().bold().to_string())
            .enumerate()
            .map(|(i, f)| (i.to_string().chars().next().unwrap(), f))
            .take(10)
            .collect::<Vec<_>>();

        let i_to_fn_id = incomings
            .iter()
            .chain(outgoings.iter())
            .filter_map(|f| self.indexer.fn_id_from_callsite(f))
            .take(10)
            .collect::<Vec<_>>();

        let header = [
            "".to_string(),
            "".to_string(),
            self.indexer.functions[f_id].pretty(&self.root),
            "".to_string(),
            "".to_string(),
        ];

        let content = std::iter::once(header)
            .chain(
                incomings
                    .iter()
                    .filter(|i| Path::new(i.0.uri.path()).starts_with(&self.root))
                    .map(|i| {
                        [
                            i.pretty(&self.root),
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
                    .filter(|o| Path::new(o.0.uri.path()).starts_with(&self.root))
                    .map(|o| {
                        [
                            "".into(),
                            "".into(),
                            "".into(),
                            "--->".to_string(),
                            o.pretty(&self.root),
                        ]
                    }),
            );

        let table = IterTable::new(content);
        let o = table.to_string();

        println!("{o}");

        while let Some(choice) = menu(
            &mut self.tty,
            explore_stack
                .iter()
                .map(|f_id| self.indexer.functions[*f_id].pretty(&self.root))
                .collect::<Vec<_>>()
                .join("\n")
                .as_str(),
            &choices,
        ) {
            match choice {
                i @ ('0'..'9') => {
                    let i = i.to_digit(10).unwrap() as usize;
                    let f_id = i_to_fn_id[i];
                    let mut explore_stack = explore_stack.clone();
                    explore_stack.push(f_id);
                    Box::pin(self.show_function(f_id, explore_stack)).await?
                }
                'q' => return Ok(()),
                _ => unreachable!(),
            }
        }

        Ok(())
    }
}
