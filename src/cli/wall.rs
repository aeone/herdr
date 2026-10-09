//! `herdr wall <target> <target> ...` — a workspace tiled with live views.
//!
//! Each target is resolved the way `herdr focus` resolves one (an agent, a
//! space or a pane), and the server opens a workspace with a view of each
//! terminal. A view draws its terminal wherever that terminal already lives
//! without resizing it, and takes the size over only while it is the pane
//! being typed into.

use crate::api::schema::{Method, Request, WorkspaceWallParams};

const USAGE: &str = "usage: herdr wall <agent|space|pane>... [--label NAME] [--no-focus]";

pub(super) fn run_wall_command(args: &[String]) -> std::io::Result<i32> {
    let parsed = match parse_args(args) {
        Ok(parsed) => parsed,
        Err(code) => return Ok(code),
    };

    let mut terminal_ids = Vec::with_capacity(parsed.targets.len());
    let mut described = Vec::with_capacity(parsed.targets.len());
    for target in &parsed.targets {
        let resolved = match super::focus::resolve(target)? {
            Ok(resolved) => resolved,
            Err(code) => return Ok(code),
        };
        terminal_ids.push(resolved.terminal_id().to_owned());
        described.push(resolved.describe());
    }

    let response = super::send_request(&Request {
        id: "cli:wall".into(),
        method: Method::WorkspaceCreateWall(WorkspaceWallParams {
            terminal_ids,
            focus: parsed.focus,
            label: parsed.label,
        }),
    })?;
    if response.get("error").is_none() {
        eprintln!("wall of {}", described.join(", "));
    }
    super::print_response(&response)
}

#[derive(Debug, PartialEq, Eq)]
struct WallArgs {
    targets: Vec<String>,
    label: Option<String>,
    focus: bool,
}

/// Parses the command line, or returns the exit code to stop with after
/// having said why.
fn parse_args(args: &[String]) -> Result<WallArgs, i32> {
    let mut parsed = WallArgs {
        targets: Vec::new(),
        label: None,
        focus: true,
    };
    let mut expect_label = false;
    for arg in args {
        if expect_label {
            parsed.label = Some(arg.clone());
            expect_label = false;
            continue;
        }
        match arg.as_str() {
            "--label" => expect_label = true,
            other if other.starts_with("--label=") => {
                parsed.label = Some(other.trim_start_matches("--label=").to_string());
            }
            "--no-focus" => parsed.focus = false,
            "--focus" => parsed.focus = true,
            "-h" | "--help" => {
                print_help();
                return Err(0);
            }
            other if other.starts_with('-') => {
                eprintln!("unknown option: {other}");
                eprintln!("{USAGE}");
                return Err(2);
            }
            other => parsed.targets.push(other.to_owned()),
        }
    }
    if expect_label {
        eprintln!("--label needs a name");
        return Err(2);
    }
    if parsed.targets.is_empty() {
        print_help();
        return Err(2);
    }
    Ok(parsed)
}

fn print_help() {
    eprintln!("{USAGE}");
    eprintln!();
    eprintln!("Opens a workspace tiled with a live view of each target. Targets are");
    eprintln!("named as for `herdr focus`: an agent, a space or a pane.");
    eprintln!();
    eprintln!("A view draws its terminal without resizing it, re-wrapped to fit the");
    eprintln!("tile. The view you type into takes the terminal's size while you do,");
    eprintln!("and gives it back when you move on.");
    eprintln!();
    eprintln!("  --label NAME  name the new workspace (default: wall)");
    eprintln!("  --no-focus    open the wall without switching to it");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn targets_and_options_are_read_in_any_order() {
        let parsed =
            parse_args(&args(&["a1", "--label", "watch", "w2", "--no-focus"])).expect("valid args");
        assert_eq!(
            parsed,
            WallArgs {
                targets: vec!["a1".into(), "w2".into()],
                label: Some("watch".into()),
                focus: false,
            }
        );
    }

    #[test]
    fn a_wall_focuses_by_default() {
        let parsed = parse_args(&args(&["a1", "--label=x"])).expect("valid args");
        assert!(parsed.focus);
        assert_eq!(parsed.label.as_deref(), Some("x"));
    }

    #[test]
    fn a_wall_of_nothing_is_rejected() {
        assert_eq!(parse_args(&args(&[])), Err(2));
        assert_eq!(parse_args(&args(&["--no-focus"])), Err(2));
    }

    #[test]
    fn a_label_without_a_name_is_rejected() {
        assert_eq!(parse_args(&args(&["a1", "--label"])), Err(2));
    }

    #[test]
    fn an_unknown_option_is_rejected() {
        assert_eq!(parse_args(&args(&["a1", "--bogus"])), Err(2));
    }
}
