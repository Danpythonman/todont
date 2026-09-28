mod alerts;
mod cli;
mod config;
mod core;
mod db;
mod due;
mod form;
mod init;
mod notify;
mod proto;
mod server;
mod service;
mod sync;
mod tui;

use clap::Parser;
use std::io::IsTerminal;

fn main() {
    let args = cli::Cli::parse();
    if let Some(cli::Command::Init {
        server,
        url,
        token,
        yes,
        force,
        ..
    }) = &args.cmd
    {
        let opts = init::Options {
            server: *server,
            url: url.clone(),
            token: token.clone(),
            yes: *yes,
            force: *force,
        };
        std::process::exit(init::run(
            &opts,
            &args.config_path(),
            &args.db_path(),
        ));
    }
    let config = match config::Config::load(&args.config_path()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("td: {e}");
            std::process::exit(78); // EX_CONFIG
        }
    };
    if let Some(cli::Command::Service { action }) = &args.cmd {
        let result = match action {
            cli::ServiceAction::Install { system } => {
                service::install(&config, scope(*system))
            }
            cli::ServiceAction::Uninstall { system } => {
                service::uninstall(scope(*system))
            }
        };
        if let Err(e) = result {
            eprintln!("td service: {e}");
            std::process::exit(1);
        }
        return;
    }
    if let Some(cli::Command::Serve) = args.cmd {
        if let Err(e) = server::run(&config) {
            eprintln!("td serve: {e}");
            std::process::exit(1);
        }
        return;
    }

    let path = args.db_path();
    let mut app = match core::App::open(&path) {
        Ok(app) => app,
        Err(e) => {
            eprintln!("td: opening {}: {e}", path.display());
            std::process::exit(e.exit_code());
        }
    };
    let code = match args.cmd {
        None => {
            if !std::io::stdout().is_terminal() {
                eprintln!("td: not a terminal; run a subcommand (see --help)");
                1
            } else {
                match tui::run(&mut app, &path, config.sync.clone()) {
                    Ok(()) => 0,
                    Err(e) => {
                        eprintln!("td: {e}");
                        1
                    }
                }
            }
        }
        Some(cmd) => cli::run(&cmd, &mut app, &config),
    };
    std::process::exit(code);
}

fn scope(system: bool) -> service::Scope {
    if system {
        service::Scope::System
    } else {
        service::Scope::User
    }
}
