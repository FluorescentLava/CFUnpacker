use anyhow::{Context, Result};
use cfunpacker::{
    detector,
    engine::{self, Progress},
    profile::Game,
};
use clap::{Parser, Subcommand};
use serde_json::{Value, json};
use std::{
    io::{self, BufRead, Write},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

#[derive(Parser)]
#[command(
    version,
    about = "CFUnpacker Rust processing core — 熔萤FluorescentLava"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    Detect {
        /// APK/APKS/XAPK input; numeric download suffixes such as .apk.1 are accepted.
        apk: PathBuf,
        #[arg(long)]
        json: bool,
    },
    Unpack {
        /// APK/APKS/XAPK input; base and asset split APKs are merged automatically.
        apk: PathBuf,
        #[arg(long)]
        output: PathBuf,
        #[arg(long, default_value = "auto")]
        game: String,
        #[arg(long)]
        overwrite: bool,
        #[arg(long, default_value_t = engine::default_workers())]
        workers: usize,
        #[arg(long)]
        json_lines: bool,
    },
    SplitBench {
        assets_root: PathBuf,
        #[arg(long)]
        output: PathBuf,
        #[arg(long)]
        game: String,
        #[arg(long, default_value_t = engine::default_workers())]
        workers: usize,
    },
}
fn emit(mut value: Value, kind: &str) {
    value["type"] = json!(kind);
    let mut out = io::stdout().lock();
    let _ = writeln!(out, "{value}");
    let _ = out.flush();
}
fn execute(cli: Cli, cancel: &AtomicBool) -> Result<()> {
    match cli.command {
        Command::Detect { apk, .. } => println!(
            "{}",
            serde_json::to_string(&detector::detect(&apk, cancel)?)?
        ),
        Command::Unpack {
            apk,
            output,
            game,
            overwrite,
            workers,
            json_lines,
        } => {
            let game = if game == "auto" {
                detector::detect(&apk, cancel)?
                    .game
                    .and_then(Game::from_id)
                    .context("自动识别失败，请手动指定 --game")?
            } else {
                Game::from_id(&game).context("未知游戏：应为 cf1/cf2/cf3/cf4/abo")?
            };
            let report = |p: Progress| {
                if json_lines {
                    emit(serde_json::to_value(p).unwrap(), "progress");
                } else {
                    eprintln!("{:5.1}% {}", p.percent, p.message);
                }
            };
            let stats = engine::unpack(&apk, &output, game, overwrite, workers, cancel, &report)?;
            emit(serde_json::to_value(stats)?, "result");
        }
        Command::SplitBench {
            assets_root,
            output,
            game,
            workers,
        } => {
            let game = Game::from_id(&game).context("未知游戏")?;
            anyhow::ensure!(
                !output.exists() || std::fs::read_dir(&output)?.next().is_none(),
                "基准输出目录必须为空"
            );
            let stats =
                engine::split_directory(&assets_root, &output, game, workers, cancel, &|_| {})?;
            let mut result = serde_json::to_value(&stats)?;
            result["atlases"] = json!(stats.atlases_decoded);
            result["frames"] = json!(stats.frames_written);
            result["skipped"] = json!(stats.skipped_items);
            emit(result, "result");
        }
    }
    Ok(())
}
fn main() {
    let cli = Cli::parse();
    let cancel = Arc::new(AtomicBool::new(false));
    let signal = Arc::clone(&cancel);
    let _ = ctrlc::set_handler(move || signal.store(true, Ordering::Relaxed));
    let stdin_cancel = Arc::clone(&cancel);
    std::thread::spawn(move || {
        for line in io::stdin().lock().lines() {
            match line {
                Ok(line) if line.trim().eq_ignore_ascii_case("cancel") => {
                    stdin_cancel.store(true, Ordering::Relaxed);
                    break;
                }
                Err(_) => break,
                _ => {}
            }
        }
    });
    if let Err(error) = execute(cli, &cancel) {
        emit(
            json!({"message": format!("{error:#}"), "cancelled": cancel.load(Ordering::Relaxed)}),
            "error",
        );
        std::process::exit(if cancel.load(Ordering::Relaxed) { 2 } else { 1 });
    }
}
