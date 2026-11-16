use crate::{
    Cancel, Site,
    engine::{Action, Engine, Event, FileOutcome, RunOptions},
    sites,
};
use anyhow::{Context, Result, bail};
use std::{path::PathBuf, sync::Arc};
pub enum Command {
    Help,
    Run(RunOptions),
}
pub fn parse(args: &[String]) -> Result<Command> {
    if args.is_empty() || args.iter().any(|s| s == "--help" || s == "-h") {
        return Ok(Command::Help);
    }
    let site = Site::parse(&args[0])?;
    let mut root = std::env::current_exe()?
        .parent()
        .context("Ejecutable sin carpeta")?
        .join("downloads");
    let mut action = Action::Download;
    let mut workers = 4;
    let mut urls = Vec::new();
    let mut args = args[1..].iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--list" => {
                if action != Action::Refresh {
                    action = Action::List
                }
            }
            "--refresh" => action = Action::Refresh,
            "-o" | "--output" => {
                root = PathBuf::from(args.next().context("Falta la carpeta de destino")?)
            }
            "--workers" => {
                workers = args
                    .next()
                    .context("Falta concurrencia")?
                    .parse::<usize>()
                    .context("Concurrencia inválida")?;
                if !(1..=16).contains(&workers) {
                    bail!("Elige entre 1 y 16 transferencias")
                }
            }
            _ if arg.starts_with('-') => bail!("Opción desconocida: {arg}"),
            _ => urls.push(arg.clone()),
        }
    }
    let urls = sites::inputs(site, &urls.join("\n"))?;
    Ok(Command::Run(RunOptions {
        site,
        urls,
        root,
        action,
        workers,
        password: None,
    }))
}
pub fn main_code() -> i32 {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(stem) = std::env::current_exe()
        .ok()
        .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().to_string()))
        && let Some(name) = stem.split('-').next()
        && Site::parse(name).is_ok()
        && !args.first().is_some_and(|s| Site::parse(s).is_ok())
    {
        args.insert(0, name.into());
    }
    let options = match parse(&args) {
        Ok(Command::Help) => {
            println!(
                "Rust Downloaders {}\nUso: recu-dl-cli [--list|--refresh] [-o carpeta] [--workers 1..16] <enlaces...>",
                env!("CARGO_PKG_VERSION")
            );
            return 0;
        }
        Ok(Command::Run(options)) => options,
        Err(e) => {
            eprintln!("Error: {e}");
            return 2;
        }
    };
    let cancel = Cancel::default();
    let token = cancel.clone();
    let sink = Arc::new(|event| match event {
        Event::Message(s) => println!("{s}"),
        Event::Listing { listing, .. } => {
            println!(
                "{}: {} archivos, {} pendientes",
                listing.title,
                listing.files.len(),
                listing.pending.len()
            );
            for file in listing.files {
                println!("  {}", file.path)
            }
        }
        Event::Progress {
            path,
            received,
            total,
        } => {
            if let Some(total) = total
                && total > 0
            {
                println!("{path}: {:.0}%", received as f64 / total as f64 * 100.0)
            }
        }
        Event::FileDone { path, outcome } => println!(
            "{}: {path}",
            match outcome {
                FileOutcome::Downloaded => "Descargado".into(),
                FileOutcome::Skipped => "Omitido".into(),
                FileOutcome::Failed(s) => format!("ERROR {s}"),
                FileOutcome::Cancelled => "Cancelado".into(),
            }
        ),
        Event::SourceChanged {
            previous_path,
            file,
        } => println!("Respaldo: {previous_path} → {}", file.path),
        Event::Finished(s) => println!(
            "Final: {} descargados, {} omitidos, {} fallidos, {} pendientes",
            s.downloaded, s.skipped, s.failed, s.pending
        ),
    });
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Error: {e}");
            return 1;
        }
    };
    let result = runtime.block_on(async {
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                token.cancel()
            }
        });
        let engine = Engine::new(options, cancel.clone(), sink)?;
        engine.run().await
    });
    match result {
        Ok(s) if s.failed == 0 && s.pending == 0 => 0,
        Ok(_) => 1,
        Err(e) => {
            eprintln!("Error: {e}");
            if cancel.is_cancelled() { 130 } else { 1 }
        }
    }
}
