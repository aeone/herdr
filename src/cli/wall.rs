//! `herdr wall <target> <target> ...` — a workspace tiled with live views.
//!
//! Each target is resolved the way `herdr focus` resolves one (an agent, a
//! space or a pane), and the server opens a workspace with a view of each
//! terminal. A view draws its terminal wherever that terminal already lives
//! without resizing it, and takes the size over only while it is the pane
//! being typed into.
//!
//! `herdr wall add <target>...` puts more views on a wall that is already
//! open.

use crate::api::schema::{Method, Request, WorkspaceWallAddParams, WorkspaceWallParams};

const USAGE: &str = "usage: herdr wall <agent|space|pane>... [--label NAME] [--no-focus]\n       herdr wall add <agent|space|pane>... [--wall WORKSPACE]";

const ADD_USAGE: &str = "usage: herdr wall add <agent|space|pane>... [--wall WORKSPACE]";

pub(super) fn run_wall_command(args: &[String]) -> std::io::Result<i32> {
    if args.first().map(String::as_str) == Some("add") {
        return run_wall_add_command(&args[1..]);
    }
    let parsed = match parse_args(args) {
        Ok(parsed) => parsed,
        Err(code) => return Ok(code),
    };

    let ResolvedTargets {
        terminal_ids,
        described,
    } = match resolve_targets(&parsed.targets)? {
        Ok(resolved) => resolved,
        Err(code) => return Ok(code),
    };

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

/// The terminals some targets name, and how each target was understood.
struct ResolvedTargets {
    terminal_ids: Vec<String>,
    described: Vec<String>,
}

/// Resolves each target as `herdr focus` would, or returns the exit code to
/// stop with after the first that matches nothing.
fn resolve_targets(targets: &[String]) -> std::io::Result<Result<ResolvedTargets, i32>> {
    let mut resolved_targets = ResolvedTargets {
        terminal_ids: Vec::with_capacity(targets.len()),
        described: Vec::with_capacity(targets.len()),
    };
    for target in targets {
        let resolved = match super::focus::resolve(target)? {
            Ok(resolved) => resolved,
            Err(code) => return Ok(Err(code)),
        };
        resolved_targets
            .terminal_ids
            .push(resolved.terminal_id().to_owned());
        resolved_targets.described.push(resolved.describe());
    }
    Ok(Ok(resolved_targets))
}

fn run_wall_add_command(args: &[String]) -> std::io::Result<i32> {
    let parsed = match parse_add_args(args) {
        Ok(parsed) => parsed,
        Err(code) => return Ok(code),
    };
    let ResolvedTargets {
        terminal_ids,
        described,
    } = match resolve_targets(&parsed.targets)? {
        Ok(resolved) => resolved,
        Err(code) => return Ok(code),
    };
    let workspace_id = match parsed.wall {
        Some(wall) => Some(resolve_wall_name(&wall)?),
        None => None,
    };

    let response = super::send_request(&Request {
        id: "cli:wall:add".into(),
        method: Method::WorkspaceWallAdd(WorkspaceWallAddParams {
            workspace_id,
            terminal_ids,
        }),
    })?;
    if response.get("error").is_none() {
        eprintln!("added {} to the wall", described.join(", "));
    }
    super::print_response(&response)
}

/// A wall named on the command line, by id or label. A label is looked up
/// here, since the server only knows workspaces by id; anything that is not
/// a label is passed on for the server to accept or refuse as an id.
fn resolve_wall_name(name: &str) -> std::io::Result<String> {
    let workspaces = super::send_request(&Request {
        id: "cli:wall:workspaces".into(),
        method: Method::WorkspaceList(Default::default()),
    })?;
    let by_label = workspaces["result"]["workspaces"]
        .as_array()
        .and_then(|workspaces| {
            workspaces
                .iter()
                .find(|workspace| workspace["label"].as_str() == Some(name))
        })
        .and_then(|workspace| workspace["workspace_id"].as_str());
    Ok(by_label.unwrap_or(name).to_owned())
}

#[derive(Debug, PartialEq, Eq)]
struct WallAddArgs {
    targets: Vec<String>,
    wall: Option<String>,
}

/// Parses `herdr wall add`'s command line, or returns the exit code to stop
/// with after having said why.
fn parse_add_args(args: &[String]) -> Result<WallAddArgs, i32> {
    let mut parsed = WallAddArgs {
        targets: Vec::new(),
        wall: None,
    };
    let mut expect_wall = false;
    for arg in args {
        if expect_wall {
            parsed.wall = Some(arg.clone());
            expect_wall = false;
            continue;
        }
        match arg.as_str() {
            "--wall" => expect_wall = true,
            other if other.starts_with("--wall=") => {
                parsed.wall = Some(other.trim_start_matches("--wall=").to_string());
            }
            "-h" | "--help" => {
                print_add_help();
                return Err(0);
            }
            other if other.starts_with('-') => {
                eprintln!("unknown option: {other}");
                eprintln!("{ADD_USAGE}");
                return Err(2);
            }
            other => parsed.targets.push(other.to_owned()),
        }
    }
    if expect_wall {
        eprintln!("--wall needs a workspace");
        return Err(2);
    }
    if parsed.targets.is_empty() {
        print_add_help();
        return Err(2);
    }
    Ok(parsed)
}

fn print_add_help() {
    eprintln!("{ADD_USAGE}");
    eprintln!();
    eprintln!("Adds a live view of each target to a wall, after the tiles it has,");
    eprintln!("and re-tiles it. Targets are named as for `herdr focus`.");
    eprintln!();
    eprintln!("  --wall WORKSPACE  the wall to add to, by id or label (default: the");
    eprintln!("                    active workspace, which must be a wall)");
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

    #[test]
    fn adding_reads_targets_and_the_wall_in_any_order() {
        let parsed = parse_add_args(&args(&["a1", "--wall", "watch", "w2"])).expect("valid args");
        assert_eq!(
            parsed,
            WallAddArgs {
                targets: vec!["a1".into(), "w2".into()],
                wall: Some("watch".into()),
            }
        );
        let parsed = parse_add_args(&args(&["--wall=w3", "a1"])).expect("valid args");
        assert_eq!(parsed.wall.as_deref(), Some("w3"));
    }

    #[test]
    fn adding_defaults_to_the_active_wall() {
        let parsed = parse_add_args(&args(&["a1"])).expect("valid args");
        assert_eq!(parsed.wall, None);
    }

    #[test]
    fn adding_nothing_or_to_no_wall_is_rejected() {
        assert_eq!(parse_add_args(&args(&[])), Err(2));
        assert_eq!(parse_add_args(&args(&["a1", "--wall"])), Err(2));
        assert_eq!(parse_add_args(&args(&["a1", "--label", "x"])), Err(2));
    }
}
