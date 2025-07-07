use anyhow::Context;
use colored::Colorize;
use dialoguer::FuzzySelect;
use spinoff::{Spinner, spinners};
use std::{
    io::Write,
    path::{Path, PathBuf},
};
use tabled::tables::IterTable;

use crate::indexing::Index;

fn menu(tty: &mut console::Term, title: &str, choices: &[(char, &str)]) -> Option<char> {
    let prompt = format!(
        "{}: {} - {}uit",
        title.white().bold(),
        choices
            .iter()
            .map(|(trigger, rest)| format!("{}{rest}", format!("[{trigger}]").yellow().bold()))
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
                'f' => self.function().await?,
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

    pub async fn function(&mut self) -> anyhow::Result<()> {
        if let Some(f_id) = FuzzySelect::new()
            .items(&self.function_names)
            .max_length(15)
            .interact_opt()?
        {
            let mut spinner = Spinner::new(spinners::Dots, "Generating...", spinoff::Color::Blue);
            let (incomings, outgoings) = self.indexer.context(f_id).await?;
            spinner.clear();

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

            let choices = incomings
                .iter()
                .chain(outgoings.iter())
                .map(|f| todo!())
                .collect::<Vec<_>>();
            while let Some(choice) = menu(&mut self.tty, "", &choices) {}
        }

        Ok(())
    }
}
