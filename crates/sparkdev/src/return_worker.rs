//! Separate process identity lets setup stop delivery without stopping jobs.
#![cfg_attr(windows, windows_subsystem = "windows")]
use clap::Parser;
use std::path::PathBuf;

#[derive(Parser)]
struct Args {
    /// Existing physical owner state; never create or migrate an identity.
    #[arg(long)]
    state_dir: Option<PathBuf>,
    /// Installer/logon dispatcher for this install's recorded owner states.
    #[arg(long, requires = "state_dir", conflicts_with = "check_state")]
    resume_autostart: bool,
    /// Validate the selected state without starting transfers.
    #[arg(long, requires = "state_dir")]
    check_state: bool,
}

fn main() {
    let args = Args::parse();
    let result = if args.resume_autostart {
        sparkdev::returns::resume_autostart(args.state_dir.as_deref().unwrap())
    } else {
        args.state_dir
            .as_deref()
            .map(sparkdev::returns::configure_worker_state)
            .transpose()
            .and_then(|_| {
                if args.check_state {
                    Ok(())
                } else {
                    sparkdev::returns::worker()
                }
            })
    };
    if let Err(error) = result {
        eprintln!("{}", error.message);
        std::process::exit(i32::from(error.code));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn startup_arguments_preserve_exact_state_and_require_it_for_dispatch() {
        let args = Args::try_parse_from([
            "worker",
            "--state-dir",
            r"C:\Owner Data\Bridge",
            "--check-state",
        ])
        .unwrap();
        assert_eq!(
            args.state_dir.unwrap(),
            PathBuf::from(r"C:\Owner Data\Bridge")
        );
        assert!(Args::try_parse_from(["worker", "--resume-autostart"]).is_err());
        assert!(
            Args::try_parse_from([
                "worker",
                "--state-dir",
                "x",
                "--resume-autostart",
                "--check-state"
            ])
            .is_err()
        );
    }
}
