//! dawdle — find the step that made your build slow, and the commit that did it.

mod cli;
mod cmd;
mod db;
mod fmt;
mod git;
mod keys;
mod ps;
mod report;
mod sampler;

use std::process::ExitCode;

use cli::Command;

/// Rust sets SIGPIPE to ignored, which turns `dawdle report | head` into a panic
/// on a broken pipe. Every other Unix tool dies quietly there, and so should
/// this one: the reader closed the pipe, which is not an error worth a backtrace.
fn reset_sigpipe() {
    #[cfg(unix)]
    // SAFETY: restoring the default disposition of SIGPIPE before any threads
    // are spawned. The only other option here is a panic, which is worse.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

fn main() -> ExitCode {
    reset_sigpipe();
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let command = match cli::parse(argv) {
        Ok(command) => command,
        Err(err) => {
            // Flag mistakes are the reader's typo, not a dawdle bug, so they get
            // a short message and no backtrace.
            eprintln!("dawdle: {err}");
            return ExitCode::from(2);
        }
    };

    let result = match command {
        Command::Help(text) => {
            println!("{text}");
            return ExitCode::SUCCESS;
        }
        Command::Version => {
            println!("dawdle {}", cli::VERSION);
            return ExitCode::SUCCESS;
        }
        Command::Run(args) => cmd::run(args).map(|code| {
            if code == 0 {
                ExitCode::SUCCESS
            } else {
                // Preserve the child's exit code: CI depends on the failing
                // build still failing after dawdle wraps it.
                ExitCode::from(u8::try_from(code).unwrap_or(1))
            }
        }),
        Command::Report(args) => cmd::report(args).map(|()| ExitCode::SUCCESS),
        Command::Blame(args) => cmd::blame(args).map(|()| ExitCode::SUCCESS),
        Command::Show(args) => cmd::show(args).map(|()| ExitCode::SUCCESS),
        Command::Ls(args) => cmd::ls(args).map(|()| ExitCode::SUCCESS),
        Command::Prune(args) => cmd::prune(args).map(|()| ExitCode::SUCCESS),
    };

    match result {
        Ok(code) => code,
        Err(err) => {
            eprintln!("dawdle: {err:#}");
            ExitCode::FAILURE
        }
    }
}
