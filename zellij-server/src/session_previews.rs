use std::path::PathBuf;
use std::time::Duration;
use zellij_utils::consts::{session_layout_cache_file_name, ZELLIJ_SOCK_DIR};
use zellij_utils::data::{SavedPanePreview, SavedSessionPreview, SavedTabPreview, SessionPreview};
use zellij_utils::input::layout::{FloatingPaneLayout, Layout, Run, TiledPaneLayout};
use zellij_utils::ipc::{
    ClientToServerMsg, IpcReceiverWithContext, IpcSenderWithContext, ServerToClientMsg,
};
use zellij_utils::session_index::layout_cwd;
use zellij_utils::sessions::{validate_session_folder_name, validate_session_name};

pub const REMOTE_PREVIEW_TIMEOUT: Duration = Duration::from_millis(1000);
pub const LOCAL_PREVIEW_ANSWER_TIMEOUT: Duration = Duration::from_millis(800);

/// How many bytes of pane contents a saved session preview may carry in total.
///
/// The preview reaches the plugin as a single event that it decodes inside its limited
/// memory, so a session with a lot of serialized scrollback would otherwise crash it.
const SAVED_PREVIEW_CONTENTS_BUDGET: usize = 1024 * 1024;

fn preview_error(session_name: &str, tab_index: Option<usize>, error: String) -> SessionPreview {
    SessionPreview {
        session_name: session_name.to_owned(),
        tab_index,
        error: Some(error),
        ..Default::default()
    }
}

fn checked_session_id(session_name: &str) -> Result<String, String> {
    validate_session_name(session_name)?;
    validate_session_folder_name(session_name)?;
    let session_id = zellij_utils::sessions::resolve_session_id(session_name)
        .unwrap_or_else(|| session_name.to_owned());
    validate_session_folder_name(&session_id)?;
    Ok(session_id)
}

pub fn running_session_preview(
    session_name: &str,
    tab_index: Option<usize>,
    pane_id: Option<(u32, bool)>,
    anchor: Option<zellij_utils::data::PreviewAnchor>,
) -> SessionPreview {
    let session_id = match checked_session_id(session_name) {
        Ok(session_id) => session_id,
        Err(e) => return preview_error(session_name, tab_index, e),
    };
    let socket_path = ZELLIJ_SOCK_DIR.join(session_id);
    let stream = match zellij_utils::consts::ipc_connect(&socket_path) {
        Ok(stream) => stream,
        Err(e) => return preview_error(session_name, tab_index, e.to_string()),
    };
    #[cfg(windows)]
    let reply_stream = match zellij_utils::consts::ipc_connect_reply(&socket_path) {
        Ok(stream) => stream,
        Err(e) => return preview_error(session_name, tab_index, e.to_string()),
    };
    let mut sender = IpcSenderWithContext::<ClientToServerMsg>::new(stream);
    if let Err(e) = sender.send_client_msg(ClientToServerMsg::RequestSessionPreview {
        tab_index: tab_index.map(|t| t as u32),
        pane_id,
        anchor,
    }) {
        return preview_error(session_name, tab_index, e.to_string());
    }
    #[cfg(windows)]
    let mut receiver: IpcReceiverWithContext<ServerToClientMsg> =
        IpcReceiverWithContext::new(reply_stream);
    #[cfg(not(windows))]
    let mut receiver: IpcReceiverWithContext<ServerToClientMsg> = sender.get_receiver();
    let _ = receiver.set_read_timeout(Some(REMOTE_PREVIEW_TIMEOUT));
    let reply = receiver.try_recv_server_msg();
    match reply {
        Ok((ServerToClientMsg::SessionPreview { preview }, _)) => match preview {
            Some(mut preview) => {
                preview.session_name = session_name.to_owned();
                preview
            },
            None => preview_error(
                session_name,
                tab_index,
                "The session could not make a preview in time".to_owned(),
            ),
        },
        Ok(_) => preview_error(
            session_name,
            tab_index,
            "The session did not send a preview".to_owned(),
        ),
        Err(e) => preview_error(session_name, tab_index, e.to_string()),
    }
}

pub fn current_session_preview(
    senders: &crate::thread_bus::ThreadSenders,
    session_name: &str,
    tab_index: Option<usize>,
    pane_id: Option<(u32, bool)>,
    anchor: Option<zellij_utils::data::PreviewAnchor>,
) -> SessionPreview {
    let (response_sender, response_receiver) = crossbeam::channel::bounded(1);
    let sent = senders.send_to_screen(crate::screen::ScreenInstruction::GetSessionPreview {
        tab_index,
        pane_id,
        anchor,
        response_channel: response_sender,
    });
    if sent.is_err() {
        return preview_error(session_name, tab_index, "The session is closing".to_owned());
    }
    match response_receiver.recv_timeout(REMOTE_PREVIEW_TIMEOUT) {
        Ok(mut preview) => {
            preview.session_name = session_name.to_owned();
            preview
        },
        Err(e) => preview_error(session_name, tab_index, e.to_string()),
    }
}

pub fn answer_preview_request(
    senders: Option<&crate::thread_bus::ThreadSenders>,
    tab_index: Option<u32>,
    pane_id: Option<(u32, bool)>,
    anchor: Option<zellij_utils::data::PreviewAnchor>,
) -> Option<SessionPreview> {
    let senders = senders?;
    let (response_sender, response_receiver) = crossbeam::channel::bounded(1);
    let sent = senders.send_to_screen(crate::screen::ScreenInstruction::GetSessionPreview {
        tab_index: tab_index.map(|t| t as usize),
        pane_id,
        anchor,
        response_channel: response_sender,
    });
    if sent.is_err() {
        return None;
    }
    response_receiver
        .recv_timeout(LOCAL_PREVIEW_ANSWER_TIMEOUT)
        .ok()
}

fn command_text(run: &Option<Run>) -> Option<String> {
    match run {
        Some(Run::Command(run_command)) => {
            let mut parts = vec![run_command.command.display().to_string()];
            parts.extend(run_command.args.iter().cloned());
            Some(parts.join(" "))
        },
        Some(Run::EditFile(path, _, _)) => Some(format!("edit {}", path.display())),
        _ => None,
    }
}

fn command_cwd(run: &Option<Run>) -> Option<String> {
    match run {
        Some(Run::Command(run_command)) => run_command
            .cwd
            .as_ref()
            .map(|cwd| cwd.display().to_string()),
        Some(Run::Cwd(cwd)) => Some(cwd.display().to_string()),
        Some(Run::EditFile(_, _, cwd)) => cwd.as_ref().map(|cwd| cwd.display().to_string()),
        _ => None,
    }
}

fn is_plugin(run: &Option<Run>) -> bool {
    matches!(run, Some(Run::Plugin(_)))
}

fn collect_tiled_panes(layout: &TiledPaneLayout, panes: &mut Vec<SavedPanePreview>) {
    if layout.children.is_empty() {
        if is_plugin(&layout.run) {
            return;
        }
        panes.push(SavedPanePreview {
            title: layout.name.clone(),
            command: command_text(&layout.run),
            cwd: command_cwd(&layout.run),
            contents: layout.pane_initial_contents.clone(),
            focused: layout.focus.unwrap_or(false),
        });
    } else {
        for child in &layout.children {
            collect_tiled_panes(child, panes);
        }
    }
}

fn collect_floating_panes(layouts: &[FloatingPaneLayout], panes: &mut Vec<SavedPanePreview>) {
    for layout in layouts {
        if is_plugin(&layout.run) {
            continue;
        }
        panes.push(SavedPanePreview {
            title: layout.name.clone(),
            command: command_text(&layout.run),
            cwd: command_cwd(&layout.run),
            contents: layout.pane_initial_contents.clone(),
            focused: layout.focus.unwrap_or(false),
        });
    }
}

/// Trims the pane contents so that together they fit in `budget` bytes.
///
/// Panes smaller than an even share keep everything and the larger ones split what is
/// left. A trimmed pane keeps its most recent lines, which is what the preview shows.
fn bound_preview_contents(tabs: &mut [SavedTabPreview], budget: usize) {
    let mut lengths: Vec<usize> = tabs
        .iter()
        .flat_map(|tab| tab.panes.iter())
        .filter_map(|pane| pane.contents.as_ref().map(|contents| contents.len()))
        .collect();
    if lengths.iter().sum::<usize>() <= budget {
        return;
    }
    lengths.sort_unstable();
    let mut remaining = budget;
    let mut cap = 0;
    for (index, length) in lengths.iter().enumerate() {
        let share = remaining / (lengths.len() - index);
        if *length > share {
            cap = share;
            break;
        }
        remaining -= length;
    }
    for contents in tabs
        .iter_mut()
        .flat_map(|tab| tab.panes.iter_mut())
        .filter_map(|pane| pane.contents.as_mut())
    {
        keep_tail(contents, cap);
    }
}

/// Drops the start of `contents` so that at most `budget` bytes are left, cutting at
/// the start of a line where possible.
fn keep_tail(contents: &mut String, budget: usize) {
    if contents.len() <= budget {
        return;
    }
    let mut cut = contents.len() - budget;
    while !contents.is_char_boundary(cut) {
        cut += 1;
    }
    if contents.as_bytes()[cut - 1] != b'\n' {
        if let Some(newline) = contents[cut..].find('\n') {
            let next_line = cut + newline + 1;
            if next_line < contents.len() {
                cut = next_line;
            }
        }
    }
    contents.drain(..cut);
}

pub fn saved_session_preview_from_layout(
    session_name: &str,
    raw_layout: &str,
    layout_path: Option<PathBuf>,
) -> SavedSessionPreview {
    let folder = layout_cwd(raw_layout).map(|cwd| cwd.display().to_string());
    match Layout::from_kdl(
        raw_layout,
        layout_path.map(|path| path.display().to_string()),
        None,
        None,
    ) {
        Ok(layout) => {
            let focused_tab = layout.focused_tab_index();
            let mut tabs: Vec<SavedTabPreview> = layout
                .tabs()
                .into_iter()
                .enumerate()
                .map(|(index, (name, tiled, floating))| {
                    let mut panes = vec![];
                    collect_tiled_panes(&tiled, &mut panes);
                    collect_floating_panes(&floating, &mut panes);
                    SavedTabPreview {
                        name: name.unwrap_or_else(|| format!("Tab #{}", index + 1)),
                        focused: focused_tab == Some(index),
                        panes,
                    }
                })
                .collect();
            bound_preview_contents(&mut tabs, SAVED_PREVIEW_CONTENTS_BUDGET);
            SavedSessionPreview {
                session_name: session_name.to_owned(),
                folder,
                tabs,
                error: None,
            }
        },
        Err(e) => SavedSessionPreview {
            session_name: session_name.to_owned(),
            folder,
            tabs: vec![],
            error: Some(format!("{}", e)),
        },
    }
}

pub fn saved_session_preview(session_name: &str) -> SavedSessionPreview {
    let session_id = match checked_session_id(session_name) {
        Ok(session_id) => session_id,
        Err(e) => {
            return SavedSessionPreview {
                session_name: session_name.to_owned(),
                error: Some(e),
                ..Default::default()
            }
        },
    };
    let layout_path = session_layout_cache_file_name(&session_id);
    match std::fs::read_to_string(&layout_path) {
        Ok(raw_layout) => {
            saved_session_preview_from_layout(session_name, &raw_layout, Some(layout_path))
        },
        Err(e) => SavedSessionPreview {
            session_name: session_name.to_owned(),
            error: Some(e.to_string()),
            ..Default::default()
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_layout_lists_tabs_panes_and_commands() {
        let folder = tempfile::tempdir().unwrap();
        std::fs::write(folder.path().join("initial_contents_1"), "$ ls\nfile\n").unwrap();
        let raw = r#"
layout {
    cwd "/home/me/project"
    tab name="editor" focus=true {
        pane command="htop" cwd="/tmp"
        pane contents_file="initial_contents_1" focus=true
    }
    tab name="logs" {
        pane command="tail" {
            args "-f" "log.txt"
        }
    }
}
"#;
        let layout_path = folder.path().join("session-layout.kdl");
        let preview = saved_session_preview_from_layout("saved", raw, Some(layout_path));
        assert_eq!(preview.error, None);
        assert_eq!(preview.folder, Some("/home/me/project".to_owned()));
        assert_eq!(preview.tabs.len(), 2);
        assert_eq!(preview.tabs[0].name, "editor");
        assert!(preview.tabs[0].focused);
        assert_eq!(preview.tabs[0].panes[0].command, Some("htop".to_owned()));
        assert_eq!(
            preview.tabs[0].panes[1].contents,
            Some("$ ls\nfile\n".to_owned())
        );
        assert_eq!(
            preview.tabs[1].panes[0].command,
            Some("tail -f log.txt".to_owned())
        );
    }

    fn numbered_lines(prefix: &str, count: usize) -> String {
        (1..=count).map(|n| format!("{} {}\n", prefix, n)).collect()
    }

    fn pane_with_contents(contents: &str) -> SavedPanePreview {
        SavedPanePreview {
            contents: Some(contents.to_owned()),
            ..Default::default()
        }
    }

    #[test]
    fn keep_tail_leaves_contents_within_the_budget_alone() {
        let mut contents = "one\ntwo\n".to_owned();
        keep_tail(&mut contents, 8);
        assert_eq!(contents, "one\ntwo\n");
    }

    #[test]
    fn keep_tail_keeps_the_last_whole_lines() {
        let mut contents = "one\ntwo\nthree\n".to_owned();
        keep_tail(&mut contents, 6);
        assert_eq!(contents, "three\n");
        let mut contents = "one\ntwo\nthree\n".to_owned();
        keep_tail(&mut contents, 10);
        assert_eq!(contents, "two\nthree\n");
    }

    #[test]
    fn keep_tail_cuts_a_single_long_line_on_a_char_boundary() {
        let mut contents = "あいうえお".to_owned();
        keep_tail(&mut contents, 7);
        assert_eq!(contents, "えお");
    }

    #[test]
    fn previews_within_the_budget_are_left_alone() {
        let mut tabs = vec![SavedTabPreview {
            panes: vec![pane_with_contents("a\nb\n"), pane_with_contents("c\n")],
            ..Default::default()
        }];
        let before = tabs.clone();
        bound_preview_contents(&mut tabs, 6);
        assert_eq!(tabs, before);
    }

    #[test]
    fn small_panes_are_kept_and_large_ones_share_the_rest() {
        let small = numbered_lines("small", 2);
        let large = numbered_lines("large", 100);
        let mut tabs = vec![
            SavedTabPreview {
                panes: vec![pane_with_contents(&small), pane_with_contents(&large)],
                ..Default::default()
            },
            SavedTabPreview {
                panes: vec![pane_with_contents(&large)],
                ..Default::default()
            },
        ];
        let budget = small.len() + 200;
        bound_preview_contents(&mut tabs, budget);
        let contents: Vec<&String> = tabs
            .iter()
            .flat_map(|tab| tab.panes.iter())
            .filter_map(|pane| pane.contents.as_ref())
            .collect();
        assert_eq!(contents[0], &small);
        for trimmed in &contents[1..] {
            assert!(trimmed.len() <= 100, "{} bytes left", trimmed.len());
            assert!(trimmed.ends_with("large 100\n"));
            assert!(trimmed.starts_with("large "));
        }
    }

    #[test]
    fn saved_previews_of_sessions_with_large_scrollback_fit_the_budget() {
        let folder = tempfile::tempdir().unwrap();
        let big = numbered_lines("line", SAVED_PREVIEW_CONTENTS_BUDGET / 8);
        std::fs::write(folder.path().join("initial_contents_1"), &big).unwrap();
        std::fs::write(folder.path().join("initial_contents_2"), &big).unwrap();
        let raw = r#"
layout {
    tab name="build" focus=true {
        pane contents_file="initial_contents_1" focus=true
        pane contents_file="initial_contents_2"
    }
}
"#;
        let layout_path = folder.path().join("session-layout.kdl");
        let preview = saved_session_preview_from_layout("big", raw, Some(layout_path));
        assert_eq!(preview.error, None);
        let panes = &preview.tabs[0].panes;
        let total: usize = panes
            .iter()
            .filter_map(|pane| pane.contents.as_ref())
            .map(|contents| contents.len())
            .sum();
        assert!(total <= SAVED_PREVIEW_CONTENTS_BUDGET, "{} bytes", total);
        let last_line = format!("line {}\n", SAVED_PREVIEW_CONTENTS_BUDGET / 8);
        for pane in panes {
            assert!(pane.contents.as_ref().unwrap().ends_with(&last_line));
        }
    }

    #[test]
    fn unreadable_layouts_report_an_error() {
        let preview = saved_session_preview_from_layout("broken", "layout {", None);
        assert!(preview.error.is_some());
    }

    #[test]
    fn saved_previews_refuse_names_that_leave_the_cache_folder() {
        for name in [
            "/tmp/zellij-preview-test",
            "../../etc",
            "..",
            ".",
            "a/b",
            "",
        ] {
            let preview = saved_session_preview(name);
            assert!(
                preview
                    .error
                    .as_deref()
                    .map(|e| e.contains("ession name"))
                    .unwrap_or(false),
                "{:?} gave {:?}",
                name,
                preview.error
            );
            assert!(preview.tabs.is_empty());
        }
    }

    #[test]
    fn running_previews_refuse_names_that_leave_the_socket_folder() {
        for name in ["/tmp/zellij-preview-test", "../x", "a/b"] {
            let preview = running_session_preview(name, None, None, None);
            assert!(
                preview
                    .error
                    .as_deref()
                    .map(|e| e.contains("ession name"))
                    .unwrap_or(false),
                "{:?} gave {:?}",
                name,
                preview.error
            );
        }
    }

    #[test]
    fn a_missing_session_reports_an_error() {
        let preview = running_session_preview("no-such-session-for-preview-test", None, None, None);
        assert!(preview.error.is_some());
    }
}
