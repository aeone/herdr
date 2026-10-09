//! `herdr wall` — open a private wall of tiles in this terminal.
//!
//! The wall itself is a client (see `client::wall`); this is its command line
//! and the list of what it can show. The list is the same set `herdr focus`
//! can target -- agents, spaces and panes -- each resolved to the terminal it
//! stands for, with that terminal's current size.

use serde_json::Value;

use crate::api::schema::{Method, Request};
use crate::client::wall::{TargetSource, WallTarget, WallTargetKind};

/// Environment telling a wall started by `herdr wall --remote` where to list
/// targets: the ssh target, and the herdr binary there. The parent opens the
/// bridge and runs the wall as a child pointed at it, the way `focus --remote`
/// runs its attach, so the client-side gates that key off the environment
/// (image paste bridging, the longer handshake wait) apply unchanged.
pub(crate) const WALL_REMOTE_TARGET_ENV_VAR: &str = "HERDR_WALL_REMOTE_TARGET";
pub(crate) const WALL_REMOTE_HERDR_ENV_VAR: &str = "HERDR_WALL_REMOTE_HERDR";

/// What the command line asked for.
#[derive(Debug, PartialEq, Eq)]
enum WallCommand {
    Open,
    Remote(String),
    /// Internal: print the target list as JSON, for a wall on the other end
    /// of ssh. Not advertised in --help, like `focus --resolve-json`.
    TargetsJson,
}

fn parse_args(args: &[String]) -> Result<WallCommand, String> {
    let mut remote: Option<String> = None;
    let mut expect_remote = false;
    let mut targets_json = false;
    for arg in args {
        if expect_remote {
            remote = Some(arg.clone());
            expect_remote = false;
            continue;
        }
        match arg.as_str() {
            "--remote" => expect_remote = true,
            other if other.starts_with("--remote=") => {
                remote = Some(other.trim_start_matches("--remote=").to_owned())
            }
            "--targets-json" => targets_json = true,
            other if other.starts_with('-') => return Err(format!("unknown option: {other}")),
            other => {
                return Err(format!(
                    "herdr wall takes no targets ({other}): it opens empty and asks what to show"
                ))
            }
        }
    }
    if expect_remote {
        return Err("--remote needs an ssh target".to_owned());
    }
    match (remote, targets_json) {
        (Some(_), true) => {
            Err("--targets-json lists this host's targets; drop --remote".to_owned())
        }
        (Some(remote), false) if remote.is_empty() || remote.starts_with('-') => {
            Err("--remote needs an ssh target".to_owned())
        }
        (Some(remote), false) => Ok(WallCommand::Remote(remote)),
        (None, true) => Ok(WallCommand::TargetsJson),
        (None, false) => Ok(WallCommand::Open),
    }
}

const WALL_HELP: &str = "\
usage: herdr wall [--remote <ssh-target>]

Opens a client of its own tiling several agents, spaces or panes, private to
it: nothing appears in the session's workspaces and other clients carry on as
they were. It starts empty and asks what to show (fzf when it is installed).

Keys, after the prefix (ctrl+b unless configured):
  g, or keys.wall_add     add a tile
  arrows, h j k l         move to the tile in that direction (or click it)
  tab / shift+tab         next / previous tile
  x                       close the active tile
  q                       quit

The active tile holds its terminal at the tile's size and takes typing, as
`herdr focus` does; the others show their terminal at its own size, re-wrapped
to fit. --remote opens it on another machine through the same bridge as
`herdr focus --remote`.";

pub(super) fn run_wall_command(args: &[String]) -> std::io::Result<i32> {
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        println!("{WALL_HELP}");
        return Ok(0);
    }
    let command = match parse_args(args) {
        Ok(command) => command,
        Err(message) => {
            eprintln!("{message}");
            eprintln!("usage: herdr wall [--remote <ssh-target>]");
            return Ok(2);
        }
    };
    match command {
        WallCommand::TargetsJson => {
            let targets = list_wall_targets()?;
            println!(
                "{}",
                serde_json::to_string(&targets).map_err(std::io::Error::other)?
            );
            Ok(0)
        }
        WallCommand::Remote(remote) => crate::remote::run_wall_remote(remote),
        WallCommand::Open => crate::client::wall::run_wall(source_from_env()),
    }
}

/// A wall run by `herdr wall --remote` lists its targets over ssh; any other
/// lists them here.
fn source_from_env() -> TargetSource {
    match (
        std::env::var(WALL_REMOTE_TARGET_ENV_VAR),
        std::env::var(WALL_REMOTE_HERDR_ENV_VAR),
    ) {
        (Ok(ssh_target), Ok(remote_herdr)) if !ssh_target.is_empty() => TargetSource::Remote {
            ssh_target,
            remote_herdr,
        },
        _ => TargetSource::Local,
    }
}

/// Everything on this host's server that a wall can show.
pub(crate) fn list_wall_targets() -> std::io::Result<Vec<WallTarget>> {
    let agents = list(Method::AgentList(Default::default()), "agents")?;
    let panes = list(Method::PaneList(Default::default()), "panes")?;
    let workspaces = list(Method::WorkspaceList(Default::default()), "workspaces")?;
    Ok(build_targets(&agents, &panes, &workspaces))
}

fn list(method: Method, field: &str) -> std::io::Result<Vec<Value>> {
    let response = super::send_request(&Request {
        id: format!("cli:wall:{field}"),
        method,
    })?;
    if let Some(error) = response.get("error") {
        return Err(std::io::Error::other(format!("listing {field}: {error}")));
    }
    Ok(response["result"][field]
        .as_array()
        .cloned()
        .unwrap_or_default())
}

/// Builds the list from the API's answers: agents first, since they are what
/// people usually want to watch, then spaces, then any pane that is not
/// already listed as an agent. A space stands for the pane `herdr focus`
/// would show for it.
fn build_targets(agents: &[Value], panes: &[Value], workspaces: &[Value]) -> Vec<WallTarget> {
    let size_of = |terminal_id: &str| -> (Option<u16>, Option<u16>) {
        panes
            .iter()
            .find(|pane| pane["terminal_id"].as_str() == Some(terminal_id))
            .map(|pane| {
                let size = &pane["size"];
                let dimension = |key: &str| {
                    size[key]
                        .as_u64()
                        .and_then(|value| u16::try_from(value).ok())
                };
                (dimension("cols"), dimension("rows"))
            })
            .unwrap_or((None, None))
    };
    let space_label = |workspace_id: &str| -> String {
        workspaces
            .iter()
            .find(|workspace| workspace["workspace_id"].as_str() == Some(workspace_id))
            .and_then(|workspace| workspace["label"].as_str())
            .unwrap_or(workspace_id)
            .to_owned()
    };

    let mut targets = Vec::new();
    for agent in agents {
        let Some(terminal_id) = agent["terminal_id"].as_str() else {
            continue;
        };
        // What can actually be typed: only a named agent has a unique name;
        // the rest share their kind, so their usable name is the pane id.
        let Some(target) = agent["name"].as_str().or_else(|| agent["pane_id"].as_str()) else {
            continue;
        };
        let kind = agent["agent"].as_str().unwrap_or("agent");
        let status = agent["agent_status"].as_str().unwrap_or("unknown");
        let title = agent["terminal_title_stripped"].as_str().unwrap_or("");
        let space = agent["workspace_id"]
            .as_str()
            .map(space_label)
            .unwrap_or_default();
        let (cols, rows) = size_of(terminal_id);
        targets.push(WallTarget {
            kind: WallTargetKind::Agent,
            target: target.to_owned(),
            terminal_id: terminal_id.to_owned(),
            label: format!("{kind} {target} · {space}"),
            detail: format!("{kind:<8} {status:<8} {space:<16} {title}"),
            cols,
            rows,
        });
    }

    for workspace in workspaces {
        let Some(workspace_id) = workspace["workspace_id"].as_str() else {
            continue;
        };
        let label = workspace["label"].as_str().unwrap_or(workspace_id);
        let Some(terminal_id) =
            super::focus::pick_space_pane(panes, workspace_id, workspace["active_tab_id"].as_str())
                .and_then(|pane| pane["terminal_id"].as_str())
        else {
            continue;
        };
        let status = workspace["agent_status"].as_str().unwrap_or("unknown");
        let (cols, rows) = size_of(terminal_id);
        targets.push(WallTarget {
            kind: WallTargetKind::Space,
            target: workspace_id.to_owned(),
            terminal_id: terminal_id.to_owned(),
            label: format!("space {label}"),
            detail: format!("{label:<28} {status}"),
            cols,
            rows,
        });
    }

    for pane in panes {
        let (Some(pane_id), Some(terminal_id)) =
            (pane["pane_id"].as_str(), pane["terminal_id"].as_str())
        else {
            continue;
        };
        if targets
            .iter()
            .any(|target| target.kind == WallTargetKind::Agent && target.terminal_id == terminal_id)
        {
            continue;
        }
        let space = pane["workspace_id"]
            .as_str()
            .map(space_label)
            .unwrap_or_default();
        let title = pane["label"]
            .as_str()
            .or_else(|| pane["terminal_title_stripped"].as_str())
            .or_else(|| pane["foreground_cwd"].as_str())
            .or_else(|| pane["cwd"].as_str())
            .unwrap_or("");
        let (cols, rows) = size_of(terminal_id);
        targets.push(WallTarget {
            kind: WallTargetKind::Pane,
            target: pane_id.to_owned(),
            terminal_id: terminal_id.to_owned(),
            label: format!("pane {pane_id} · {space}"),
            detail: format!("{space:<16} {title}"),
            cols,
            rows,
        });
    }
    targets
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn a_bare_wall_opens_locally() {
        assert_eq!(parse_args(&args(&[])), Ok(WallCommand::Open));
    }

    #[test]
    fn remote_takes_an_ssh_target_in_either_form() {
        assert_eq!(
            parse_args(&args(&["--remote", "lute"])),
            Ok(WallCommand::Remote("lute".into()))
        );
        assert_eq!(
            parse_args(&args(&["--remote=me@lute"])),
            Ok(WallCommand::Remote("me@lute".into()))
        );
    }

    #[test]
    fn remote_without_a_target_is_refused() {
        assert!(parse_args(&args(&["--remote"])).is_err());
        assert!(parse_args(&args(&["--remote", "--targets-json"])).is_err());
        assert!(parse_args(&args(&["--remote="])).is_err());
    }

    #[test]
    fn targets_are_picked_inside_the_wall_not_on_the_command_line() {
        assert!(parse_args(&args(&["claude1"])).is_err());
        assert!(parse_args(&args(&["--label", "x"])).is_err());
    }

    #[test]
    fn the_target_list_is_only_this_hosts() {
        assert_eq!(
            parse_args(&args(&["--targets-json"])),
            Ok(WallCommand::TargetsJson)
        );
        assert!(parse_args(&args(&["--targets-json", "--remote", "lute"])).is_err());
    }

    #[test]
    fn wrong_arguments_exit_with_usage() {
        assert_eq!(
            run_wall_command(&args(&["--bogus"])).expect("no io error"),
            2
        );
    }

    fn pane(pane_id: &str, workspace_id: &str, terminal_id: &str, focused: bool) -> Value {
        serde_json::json!({
            "pane_id": pane_id,
            "workspace_id": workspace_id,
            "tab_id": format!("{workspace_id}:t1"),
            "terminal_id": terminal_id,
            "focused": focused,
            "cwd": "/src",
            "size": {"cols": 100, "rows": 30},
        })
    }

    #[test]
    fn agents_spaces_and_other_panes_are_all_listed_once_each() {
        let agents = vec![serde_json::json!({
            "terminal_id": "t1",
            "name": "claude1",
            "agent": "claude",
            "agent_status": "working",
            "pane_id": "w1:p1",
            "workspace_id": "w1",
        })];
        let panes = vec![
            pane("w1:p1", "w1", "t1", true),
            pane("w1:p2", "w1", "t2", false),
            pane("w2:p1", "w2", "t3", false),
        ];
        let workspaces = vec![
            serde_json::json!({"workspace_id": "w1", "label": "parser", "active_tab_id": "w1:t1", "agent_status": "working"}),
            serde_json::json!({"workspace_id": "w2", "label": "docs", "active_tab_id": "w2:t1", "agent_status": "idle"}),
        ];

        let targets = build_targets(&agents, &panes, &workspaces);
        let summary: Vec<(WallTargetKind, &str, &str)> = targets
            .iter()
            .map(|target| {
                (
                    target.kind,
                    target.target.as_str(),
                    target.terminal_id.as_str(),
                )
            })
            .collect();

        assert_eq!(
            summary,
            vec![
                (WallTargetKind::Agent, "claude1", "t1"),
                (WallTargetKind::Space, "w1", "t1"),
                (WallTargetKind::Space, "w2", "t3"),
                // w1:p1 is the agent already listed.
                (WallTargetKind::Pane, "w1:p2", "t2"),
                (WallTargetKind::Pane, "w2:p1", "t3"),
            ]
        );
        assert_eq!(targets[0].label, "claude claude1 · parser");
        assert_eq!(targets[0].size(), Some((100, 30)));
    }

    /// A server from before panes reported their size still lists, and the
    /// wall guesses the size instead.
    #[test]
    fn a_pane_without_a_size_is_listed_without_one() {
        let mut bare = pane("w1:p1", "w1", "t1", true);
        bare.as_object_mut().expect("object").remove("size");

        let targets = build_targets(&[], &[bare], &[]);

        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].size(), None);
    }
}
