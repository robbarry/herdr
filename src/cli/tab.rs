use std::collections::HashMap;

use crate::api::schema::{TabCreateParams, TabListParams, TabNameForPaneParams, TabRenameParams};

pub(super) fn run_tab_command(args: &[String]) -> std::io::Result<i32> {
    let Some(subcommand) = args.first().map(|arg| arg.as_str()) else {
        print_tab_help();
        return Ok(2);
    };

    match subcommand {
        "list" => tab_list(&args[1..]),
        "create" => tab_create(&args[1..]),
        "get" => tab_get(&args[1..]),
        "focus" => tab_focus(&args[1..]),
        "rename" => tab_rename(&args[1..]),
        "name-for-pane" => tab_name_for_pane(&args[1..]),
        "close" => tab_close(&args[1..]),
        "help" | "--help" | "-h" => {
            print_tab_help();
            Ok(0)
        }
        _ => {
            print_tab_help();
            Ok(2)
        }
    }
}

fn tab_list(args: &[String]) -> std::io::Result<i32> {
    let mut workspace_id = None;

    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--workspace" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --workspace");
                    return Ok(2);
                };
                workspace_id = Some(super::normalize_workspace_id(value));
                index += 2;
            }
            other => {
                eprintln!("unknown option: {other}");
                return Ok(2);
            }
        }
    }

    super::runtime::tab_list(TabListParams { workspace_id })
}

fn tab_create(args: &[String]) -> std::io::Result<i32> {
    let mut workspace_id = None;
    let mut cwd = None;
    let mut focus = false;
    let mut label = None;
    let mut env = HashMap::new();

    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--workspace" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --workspace");
                    return Ok(2);
                };
                workspace_id = Some(super::normalize_workspace_id(value));
                index += 2;
            }
            "--cwd" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --cwd");
                    return Ok(2);
                };
                cwd = Some(value.clone());
                index += 2;
            }
            "--label" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --label");
                    return Ok(2);
                };
                label = Some(value.clone());
                index += 2;
            }
            "--focus" => {
                focus = true;
                index += 1;
            }
            "--no-focus" => {
                focus = false;
                index += 1;
            }
            "--env" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --env");
                    return Ok(2);
                };
                let (key, value) = match super::parse_env_assignment(value) {
                    Ok(pair) => pair,
                    Err(err) => {
                        eprintln!("{err}");
                        return Ok(2);
                    }
                };
                env.insert(key, value);
                index += 2;
            }
            other => {
                eprintln!("unknown option: {other}");
                return Ok(2);
            }
        }
    }

    super::runtime::tab_create(TabCreateParams {
        workspace_id,
        cwd,
        focus,
        label,
        env,
    })
}

fn tab_get(args: &[String]) -> std::io::Result<i32> {
    let Some(raw_tab_id) = args.first() else {
        eprintln!("usage: herdr tab get <tab_id>");
        return Ok(2);
    };
    if args.len() != 1 {
        eprintln!("usage: herdr tab get <tab_id>");
        return Ok(2);
    }

    super::runtime::tab_get(super::normalize_tab_id(raw_tab_id))
}

fn tab_focus(args: &[String]) -> std::io::Result<i32> {
    let Some(raw_tab_id) = args.first() else {
        eprintln!("usage: herdr tab focus <tab_id>");
        return Ok(2);
    };
    if args.len() != 1 {
        eprintln!("usage: herdr tab focus <tab_id>");
        return Ok(2);
    }

    super::runtime::tab_focus(super::normalize_tab_id(raw_tab_id))
}

fn tab_rename(args: &[String]) -> std::io::Result<i32> {
    if args.len() < 2 {
        eprintln!("usage: herdr tab rename <tab_id> <label>");
        return Ok(2);
    }

    super::runtime::tab_rename(TabRenameParams {
        tab_id: super::normalize_tab_id(&args[0]),
        label: args[1..].join(" "),
    })
}

fn tab_name_for_pane(args: &[String]) -> std::io::Result<i32> {
    const USAGE: &str =
        "usage: herdr tab name-for-pane <pane_id> <label> [--even-if-named] [--even-if-shared]";
    let mut pane_id = None;
    let mut label_words = Vec::new();
    let mut if_auto_named = true;
    let mut if_single_pane = true;

    for arg in args {
        match arg.as_str() {
            "--even-if-named" => if_auto_named = false,
            "--even-if-shared" => if_single_pane = false,
            other if other.starts_with("--") => {
                eprintln!("unknown option: {other}");
                return Ok(2);
            }
            other if pane_id.is_none() => pane_id = Some(super::normalize_pane_id(other)),
            other => label_words.push(other),
        }
    }
    let (Some(pane_id), false) = (pane_id, label_words.is_empty()) else {
        eprintln!("{USAGE}");
        return Ok(2);
    };

    super::runtime::tab_name_for_pane(TabNameForPaneParams {
        pane_id,
        label: label_words.join(" "),
        if_auto_named,
        if_single_pane,
    })
}

fn tab_close(args: &[String]) -> std::io::Result<i32> {
    let Some(raw_tab_id) = args.first() else {
        eprintln!("usage: herdr tab close <tab_id>");
        return Ok(2);
    };
    if args.len() != 1 {
        eprintln!("usage: herdr tab close <tab_id>");
        return Ok(2);
    }

    super::runtime::tab_close(super::normalize_tab_id(raw_tab_id))
}

fn print_tab_help() {
    eprintln!("herdr tab commands:");
    eprintln!("  herdr tab list [--workspace <workspace_id>]");
    eprintln!(
        "  herdr tab create [--workspace <workspace_id>] [--cwd PATH] [--label TEXT] [--env KEY=VALUE] [--focus] [--no-focus]"
    );
    eprintln!("  herdr tab get <tab_id>");
    eprintln!("  herdr tab focus <tab_id>");
    eprintln!("  herdr tab rename <tab_id> <label>");
    eprintln!("  herdr tab name-for-pane <pane_id> <label> [--even-if-named] [--even-if-shared]");
    eprintln!("  herdr tab close <tab_id>");
}
