use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use rustix::process::{Pid, getpgid, test_kill_process_group};

use super::*;
use crate::temp_workspace::TempWorkspace;

const ADDRESS: &str = "ws://127.0.0.1:9/devtools/browser/fake";

/// Подменный «браузер» — настоящий дочерний процесс со своими потомками.
fn fake_browser(workspace: &TempWorkspace, name: &str, body: &str) -> BrowserLaunchSpec {
    let path = workspace.path().join(name);
    fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    BrowserLaunchSpec {
        executable: path,
        user_data_dir: workspace.path().join("profile"),
        tmpdir: workspace.path().display().to_string(),
        launch_timeout: Duration::from_secs(5),
    }
}

fn group_alive(group: i32) -> bool {
    match test_kill_process_group(Pid::from_raw(group).unwrap()) {
        Ok(()) => true,
        Err(rustix::io::Errno::SRCH) => false,
        Err(error) => panic!("проверка группы: {error}"),
    }
}

async fn wait_group_gone(group: i32) {
    for _ in 0..200 {
        if !group_alive(group) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("группа {group} не завершилась");
}

#[tokio::test]
async fn browser_runs_in_its_own_group_and_stop_kills_descendants() {
    let workspace = TempWorkspace::create("browser-process-group-test").unwrap();
    let spec = fake_browser(
        &workspace,
        "browser",
        &format!("sleep 300 &\necho 'DevTools listening on {ADDRESS}' >&2\nexec sleep 300"),
    );
    let (mut process, address) = OwnedBrowserProcess::spawn(&spec).await.unwrap();
    assert_eq!(address, ADDRESS);
    let pid = Pid::from_raw(process.pid()).unwrap();
    // Лидер своей группы: терминальный сигнал foreground-группы CLI до него не доходит.
    assert_eq!(getpgid(Some(pid)).unwrap(), pid);
    assert_ne!(getpgid(None).unwrap(), pid);
    assert!(group_alive(process.pid()));
    assert_eq!(
        process.stop(Duration::from_secs(2)).await,
        Ok(ProcessStop::Killed)
    );
    // Потомок лидера в той же группе тоже остановлен: группа пуста.
    assert!(!group_alive(process.pid()));
    workspace.close().unwrap();
}

#[tokio::test]
async fn leader_exit_with_lingering_descendant_drains_only_owned_group() {
    let workspace = TempWorkspace::create("browser-process-drain-test").unwrap();
    let spec = fake_browser(
        &workspace,
        "browser",
        &format!("sleep 300 &\necho 'DevTools listening on {ADDRESS}' >&2\nsleep 0.2\nexit 0"),
    );
    let (mut process, _) = OwnedBrowserProcess::spawn(&spec).await.unwrap();
    let group = process.pid();
    let status = process
        .wait_exit(Duration::from_secs(5))
        .await
        .unwrap()
        .expect("лидер завершается сам");
    assert!(status.success());
    // Самостоятельный выход лидера не доказывает остановку его потомков: их
    // завершает владелец группы до признания процесса остановленным.
    assert!(!group_alive(group));
    assert_eq!(
        process.stop(Duration::from_secs(2)).await,
        Ok(ProcessStop::Exited)
    );
    workspace.close().unwrap();
}

#[tokio::test]
async fn leader_exit_before_address_still_drains_descendants_within_limit() {
    let workspace = TempWorkspace::create("browser-process-early-exit-test").unwrap();
    let spec = fake_browser(&workspace, "browser", "sleep 300 &\nexit 1");
    let failure = OwnedBrowserProcess::spawn(&spec).await.unwrap_err();
    assert!(
        failure.message.contains("до адреса DevTools"),
        "{}",
        failure.message
    );
    // Лидер вышел сам, а оставшийся потомок собран в пределах лимита остановки.
    assert_eq!(failure.stop, Ok(ProcessStop::Exited));
    workspace.close().unwrap();
}

#[tokio::test]
async fn launch_timeout_stops_the_group_and_reports_typed_stop() {
    let workspace = TempWorkspace::create("browser-process-timeout-test").unwrap();
    let mut spec = fake_browser(&workspace, "browser", "sleep 300 &\nexec sleep 300");
    spec.launch_timeout = Duration::from_millis(200);
    let failure = OwnedBrowserProcess::spawn(&spec).await.unwrap_err();
    assert!(
        failure
            .message
            .contains("истекло время ожидания адреса DevTools")
    );
    assert_eq!(failure.stop, Ok(ProcessStop::Killed));
    workspace.close().unwrap();
}

#[tokio::test]
async fn exit_before_address_keeps_bounded_safe_stderr() {
    let workspace = TempWorkspace::create("browser-process-exit-test").unwrap();
    let spec = fake_browser(
        &workspace,
        "browser",
        "echo 'отказ инициализации' >&2\nexit 3",
    );
    let failure = OwnedBrowserProcess::spawn(&spec).await.unwrap_err();
    assert!(
        failure.message.contains("до адреса DevTools"),
        "{}",
        failure.message
    );
    assert!(failure.message.contains("отказ инициализации"));
    assert_eq!(failure.stop, Ok(ProcessStop::Exited));
    workspace.close().unwrap();
}

#[tokio::test]
async fn missing_executable_is_a_setup_error_without_process() {
    let workspace = TempWorkspace::create("browser-process-missing-test").unwrap();
    let spec = BrowserLaunchSpec {
        executable: workspace.path().join("missing"),
        user_data_dir: workspace.path().join("profile"),
        tmpdir: workspace.path().display().to_string(),
        launch_timeout: Duration::from_secs(1),
    };
    let failure = OwnedBrowserProcess::spawn(&spec).await.unwrap_err();
    assert!(failure.message.starts_with("запуск браузера:"));
    assert_eq!(failure.stop, Ok(ProcessStop::Exited));
    workspace.close().unwrap();
}

#[tokio::test]
async fn dropping_unclosed_process_kills_only_its_group() {
    let workspace = TempWorkspace::create("browser-process-drop-test").unwrap();
    let spec = fake_browser(
        &workspace,
        "browser",
        &format!("sleep 300 &\necho 'DevTools listening on {ADDRESS}' >&2\nexec sleep 300"),
    );
    let (process, _) = OwnedBrowserProcess::spawn(&spec).await.unwrap();
    let group = process.pid();
    drop(process);
    wait_group_gone(group).await;
    // Текущая группа теста не затронута.
    assert!(group_alive(getpgid(None).unwrap().as_raw_nonzero().get()));
    workspace.close().unwrap();
}

#[test]
fn arguments_keep_chromiumoxide_defaults_and_session_options() {
    let spec = BrowserLaunchSpec {
        executable: "/bin/true".into(),
        user_data_dir: "/tmp/профиль".into(),
        tmpdir: "t".into(),
        launch_timeout: Duration::from_secs(1),
    };
    let arguments = spec.arguments();
    for expected in DEFAULT_ARGS {
        assert!(arguments.iter().any(|argument| argument == expected));
    }
    for expected in [
        "--remote-debugging-port=0",
        "--disable-extensions",
        "--user-data-dir=/tmp/профиль",
        "--headless",
        "--incognito",
    ] {
        assert!(
            arguments.iter().any(|argument| argument == expected),
            "{expected}"
        );
    }
    assert!(!arguments.iter().any(|argument| argument == "--no-sandbox"));
}

#[test]
fn devtools_address_requires_browser_websocket() {
    assert_eq!(
        parse_devtools_address(&format!("DevTools listening on {ADDRESS}\n")),
        Some(ADDRESS.into())
    );
    assert_eq!(
        parse_devtools_address("DevTools listening on http://127.0.0.1:9/json"),
        None
    );
    assert_eq!(
        parse_devtools_address("listening on ws://x/devtools/page/1"),
        None
    );
}
