//! `herdr focus <target>` — open a client pinned to one thing.
//!
//! A wrapper over the existing per-client terminal modes rather than new server
//! state. `App` clients all render from the same session state, so they follow
//! each other's focus; a terminal client renders one terminal independently,
//! which is what "show me this and ignore what the others are doing" means. The
//! remote-space mirrors already rely on that same independence.
//!
//! The wrapping is the useful part: `terminal attach` wants a terminal id, and
//! nobody knows those. This resolves what a person would actually name — an
//! agent, a space, or a pane.

use serde_json::Value;

use crate::api::schema::{Method, Request};

/// What a target string resolved to, kept for the message printed on attach.
enum Resolved {
    Agent { terminal_id: String, label: String },
    Pane { terminal_id: String, label: String },
    Space { terminal_id: String, label: String },
}

impl Resolved {
    fn terminal_id(&self) -> &str {
        match self {
            Self::Agent { terminal_id, .. }
            | Self::Pane { terminal_id, .. }
            | Self::Space { terminal_id, .. } => terminal_id,
        }
    }

    fn describe(&self) -> String {
        match self {
            Self::Agent { label, .. } => format!("agent {label}"),
            Self::Pane { label, .. } => format!("pane {label}"),
            Self::Space { label, .. } => format!("space {label}"),
        }
    }
}

pub(super) fn run_focus_command(args: &[String]) -> std::io::Result<i32> {
    let mut target = None;
    let mut takeover = false;
    let mut observe = false;
    for arg in args {
        match arg.as_str() {
            "--takeover" => takeover = true,
            "--observe" => observe = true,
            other if other.starts_with('-') => {
                eprintln!("unknown option: {other}");
                return Ok(2);
            }
            other => {
                if target.replace(other.to_owned()).is_some() {
                    eprintln!("focus takes a single target");
                    return Ok(2);
                }
            }
        }
    }
    let Some(target) = target else {
        eprintln!("usage: herdr focus <agent|space|pane> [--observe] [--takeover]");
        eprintln!();
        eprintln!("Opens a client showing just that one thing, independent of what any");
        eprintln!("other herdr client is focused on. Detach with ctrl+b q.");
        eprintln!();
        return print_targets();
    };
    if observe && takeover {
        eprintln!("--observe is read-only, so it cannot take over");
        return Ok(2);
    }

    let resolved = match resolve(&target)? {
        Ok(resolved) => resolved,
        Err(code) => return Ok(code),
    };

    eprintln!("focusing {} — detach with ctrl+b q", resolved.describe());
    if observe {
        // Observe is the read-only mode, so the focused view cannot type into
        // whatever it is watching.
        // 0 means "size from this terminal", the same default the
        // `terminal session observe` command uses.
        crate::client::run_terminal_session_observe(resolved.terminal_id().to_owned(), 0, 0)?;
    } else {
        crate::client::run_terminal_attach(resolved.terminal_id().to_owned(), takeover)?;
    }
    Ok(0)
}

/// Lists what can be focused, so the bare command is a menu rather than a
/// scolding. Agents first: they are what people usually mean, and a space is
/// often reachable by the agent inside it anyway.
fn print_targets() -> std::io::Result<i32> {
    let agents = crate::cli::send_request(&Request {
        id: "cli:focus:list-agents".into(),
        method: Method::AgentList(Default::default()),
    })?;
    let agents = agents["result"]["agents"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if agents.is_empty() {
        eprintln!("agents: none running");
    } else {
        eprintln!("agents:");
        for agent in &agents {
            // Show what can actually be typed. Only named agents have a
            // unique name; the rest share their kind ("claude"), so their
            // usable target is the pane id.
            let target = agent["name"]
                .as_str()
                .or_else(|| agent["pane_id"].as_str())
                .unwrap_or("?");
            let kind = agent["agent"].as_str().unwrap_or("");
            let status = agent["agent_status"].as_str().unwrap_or("unknown");
            let title = agent["terminal_title_stripped"].as_str().unwrap_or("");
            eprintln!(
                "  {target:<10} {kind:<8} {status:<8} {}",
                truncate(title, 42)
            );
        }
    }

    let workspaces = crate::cli::send_request(&Request {
        id: "cli:focus:list-spaces".into(),
        method: Method::WorkspaceList(Default::default()),
    })?;
    let workspaces = workspaces["result"]["workspaces"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if !workspaces.is_empty() {
        eprintln!();
        eprintln!("spaces:");
        for workspace in &workspaces {
            let id = workspace["workspace_id"].as_str().unwrap_or("?");
            let label = workspace["label"].as_str().unwrap_or("");
            let status = workspace["agent_status"].as_str().unwrap_or("unknown");
            eprintln!("  {id:<6} {:<28} {status}", truncate(label, 28));
        }
    }
    // Not an error: the user asked what they could focus and got an answer.
    Ok(0)
}

fn truncate(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_string();
    }
    let kept: String = value.chars().take(max.saturating_sub(1)).collect();
    format!("{kept}…")
}

/// Resolves a human-typed target, most specific interpretation first.
///
/// An agent name is tried before a space name because agents are what people
/// name deliberately; space labels are often derived from a directory and
/// collide more.
fn resolve(target: &str) -> std::io::Result<Result<Resolved, i32>> {
    if let Some(resolved) = resolve_agent(target)? {
        return Ok(Ok(resolved));
    }
    if let Some(resolved) = resolve_pane_or_space(target)? {
        return Ok(Ok(resolved));
    }
    eprintln!("no agent, space or pane matched {target:?}");
    eprintln!("try: herdr agent list   or   herdr workspace list");
    Ok(Err(1))
}

fn resolve_agent(target: &str) -> std::io::Result<Option<Resolved>> {
    let response = crate::cli::send_request(&Request {
        id: "cli:focus:agent".into(),
        method: Method::AgentGet(crate::api::schema::AgentTarget {
            target: target.to_owned(),
        }),
    })?;
    if response.get("error").is_some() {
        return Ok(None);
    }
    let agent = &response["result"]["agent"];
    let Some(terminal_id) = agent["terminal_id"].as_str() else {
        return Ok(None);
    };
    let label = agent["name"]
        .as_str()
        .or_else(|| agent["agent"].as_str())
        .unwrap_or(target);
    Ok(Some(Resolved::Agent {
        terminal_id: terminal_id.to_owned(),
        label: label.to_owned(),
    }))
}

/// Matches a pane id outright, or a space by id or label and then picks the
/// pane a person would expect to be looking at.
fn resolve_pane_or_space(target: &str) -> std::io::Result<Option<Resolved>> {
    let panes = crate::cli::send_request(&Request {
        id: "cli:focus:panes".into(),
        method: Method::PaneList(Default::default()),
    })?;
    let Some(panes) = panes["result"]["panes"].as_array() else {
        return Ok(None);
    };

    if let Some(pane) = panes
        .iter()
        .find(|pane| pane["pane_id"].as_str() == Some(target))
    {
        if let Some(terminal_id) = pane["terminal_id"].as_str() {
            return Ok(Some(Resolved::Pane {
                terminal_id: terminal_id.to_owned(),
                label: target.to_owned(),
            }));
        }
    }

    let workspaces = crate::cli::send_request(&Request {
        id: "cli:focus:workspaces".into(),
        method: Method::WorkspaceList(Default::default()),
    })?;
    let Some(workspaces) = workspaces["result"]["workspaces"].as_array() else {
        return Ok(None);
    };
    let workspace = workspaces.iter().find(|workspace| {
        workspace["workspace_id"].as_str() == Some(target)
            || workspace["label"].as_str() == Some(target)
    });
    let Some(workspace) = workspace else {
        return Ok(None);
    };
    let workspace_id = workspace["workspace_id"].as_str().unwrap_or_default();
    let label = workspace["label"].as_str().unwrap_or(target);

    let pane = pick_space_pane(panes, workspace_id, workspace["active_tab_id"].as_str());
    let Some(terminal_id) = pane.and_then(|pane| pane["terminal_id"].as_str()) else {
        eprintln!("space {label} has no pane to focus");
        return Ok(None);
    };
    Ok(Some(Resolved::Space {
        terminal_id: terminal_id.to_owned(),
        label: label.to_owned(),
    }))
}

/// The pane in a space worth showing: the focused one, else anything in the
/// active tab, else the first pane the space has.
///
/// A space can hold several panes across several tabs and this shows exactly
/// one of them, which is the accepted limit of wrapping `terminal attach`
/// rather than teaching the server about per-client spaces.
fn pick_space_pane<'a>(
    panes: &'a [Value],
    workspace_id: &str,
    active_tab_id: Option<&str>,
) -> Option<&'a Value> {
    let in_space = |pane: &&Value| pane["workspace_id"].as_str() == Some(workspace_id);
    panes
        .iter()
        .filter(in_space)
        .find(|pane| pane["focused"].as_bool() == Some(true))
        .or_else(|| {
            panes
                .iter()
                .filter(in_space)
                .find(|pane| active_tab_id.is_some() && pane["tab_id"].as_str() == active_tab_id)
        })
        .or_else(|| panes.iter().find(in_space))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(pane_id: &str, workspace_id: &str, tab_id: &str, focused: bool) -> Value {
        serde_json::json!({
            "pane_id": pane_id,
            "workspace_id": workspace_id,
            "tab_id": tab_id,
            "focused": focused,
            "terminal_id": format!("term_{pane_id}"),
        })
    }

    #[test]
    fn a_spaces_focused_pane_wins() {
        let panes = vec![
            pane("w1:p1", "w1", "w1:t1", false),
            pane("w1:p2", "w1", "w1:t1", true),
            pane("w2:p1", "w2", "w2:t1", true),
        ];

        let picked = pick_space_pane(&panes, "w1", Some("w1:t1")).expect("pane");

        assert_eq!(picked["pane_id"], "w1:p2");
    }

    /// Nothing focused in that space — a space the user has never visited — so
    /// fall back to its active tab rather than giving up.
    #[test]
    fn an_unfocused_space_falls_back_to_its_active_tab() {
        let panes = vec![
            pane("w1:p1", "w1", "w1:t1", false),
            pane("w1:p9", "w1", "w1:t2", false),
        ];

        let picked = pick_space_pane(&panes, "w1", Some("w1:t2")).expect("pane");

        assert_eq!(picked["pane_id"], "w1:p9");
    }

    #[test]
    fn a_space_with_no_active_tab_still_resolves() {
        let panes = vec![pane("w3:p1", "w3", "w3:t1", false)];

        let picked = pick_space_pane(&panes, "w3", None).expect("pane");

        assert_eq!(picked["pane_id"], "w3:p1");
    }

    #[test]
    fn a_space_with_no_panes_resolves_to_nothing() {
        let panes = vec![pane("w1:p1", "w1", "w1:t1", true)];

        assert!(pick_space_pane(&panes, "w2", None).is_none());
    }
}
