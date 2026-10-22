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

/// Та же проверка, что у владельца группы: несобранные зомби не считаются.
fn group_alive(group: i32) -> bool {
    match test_kill_process_group(Pid::from_raw(group).unwrap()) {
        Ok(()) => group_has_live_members(Path::new(PROC_ROOT), group, std::process::id())
            .unwrap_or_else(|error| panic!("проверка группы: {error}")),
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
async fn exit_before_address_keeps_unterminated_stderr_held_open_by_descendant() {
    let workspace = TempWorkspace::create("browser-process-partial-stderr-test").unwrap();
    // Потомок держит stderr открытым, а строка отказа не завершена переводом строки.
    let spec = fake_browser(
        &workspace,
        "browser",
        "sleep 300 &\nprintf 'частичный отказ' >&2\nexit 3",
    );
    let failure = OwnedBrowserProcess::spawn(&spec).await.unwrap_err();
    assert!(
        failure.message.contains("частичный отказ"),
        "{}",
        failure.message
    );
    assert_eq!(failure.stop, Ok(ProcessStop::Exited));
    workspace.close().unwrap();
}

#[tokio::test]
async fn long_unterminated_stderr_before_address_is_parsed_with_bounded_buffers() {
    let workspace = TempWorkspace::create("browser-process-long-stderr-test").unwrap();
    // Мегабайт без перевода строки, затем адрес отдельной строкой.
    let spec = fake_browser(
        &workspace,
        "browser",
        &format!(
            "head -c 1048576 /dev/zero | tr '\\0' x >&2\necho >&2\necho 'DevTools listening on {ADDRESS}' >&2\nexec sleep 300"
        ),
    );
    let (mut process, address) = OwnedBrowserProcess::spawn(&spec).await.unwrap();
    assert_eq!(address, ADDRESS);
    assert_eq!(
        process.stop(Duration::from_secs(2)).await,
        Ok(ProcessStop::Killed)
    );
    workspace.close().unwrap();
}

#[tokio::test]
async fn long_unterminated_stderr_before_exit_keeps_bounded_diagnostics() {
    let workspace = TempWorkspace::create("browser-process-long-exit-test").unwrap();
    let spec = fake_browser(
        &workspace,
        "browser",
        "printf 'начало отказа' >&2\nhead -c 1048576 /dev/zero | tr '\\0' x >&2\nexit 3",
    );
    let failure = OwnedBrowserProcess::spawn(&spec).await.unwrap_err();
    assert!(
        failure.message.contains("до адреса DevTools"),
        "{}",
        failure.message
    );
    assert!(failure.message.contains("начало отказа"));
    assert!(failure.message.len() < 4 * MAX_LAUNCH_STDERR_BYTES);
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
async fn stderr_eof_before_reapable_exit_is_a_natural_exit() {
    let workspace = TempWorkspace::create("browser-process-eof-exit-test").unwrap();
    // EOF наступает заметно раньше выхода лидера — растянутое окно той же гонки,
    // что между закрытием дескрипторов и доступностью статуса для `wait`.
    let spec = fake_browser(
        &workspace,
        "browser",
        "echo 'отказ до выхода' >&2\nexec 2>&-\nsleep 0.1\nexit 3",
    );
    let failure = OwnedBrowserProcess::spawn(&spec).await.unwrap_err();
    assert!(
        failure
            .message
            .contains("процесс завершился (exit status: 3)"),
        "{}",
        failure.message
    );
    assert!(failure.message.contains("отказ до выхода"));
    assert_eq!(failure.stop, Ok(ProcessStop::Exited));
    workspace.close().unwrap();
}

#[tokio::test]
async fn closed_stderr_with_live_leader_is_stopped_by_force() {
    let workspace = TempWorkspace::create("browser-process-eof-alive-test").unwrap();
    let spec = fake_browser(
        &workspace,
        "browser",
        "echo 'stderr закрыт' >&2\nsleep 300 2>&- &\nexec 2>&-\nexec sleep 300",
    );
    let started = Instant::now();
    let failure = OwnedBrowserProcess::spawn(&spec).await.unwrap_err();
    assert!(
        failure.message.contains("stderr закрыт до адреса DevTools"),
        "{}",
        failure.message
    );
    // Живой лидер и его потомок не выдаются за самостоятельный выход и не
    // удерживают запуск до общего таймаута.
    assert_eq!(failure.stop, Ok(ProcessStop::Killed));
    assert!(started.elapsed() < spec.launch_timeout);
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

/// Буферы ограничены по объёму удерживаемой памяти, а не только по длине
/// итогового сообщения: проверяются длина и ёмкость при любой нарезке потока.
fn assert_bounded(output: &LaunchStderr) {
    assert!(output.line.len() <= MAX_LAUNCH_LINE_BYTES);
    assert!(output.line.capacity() <= 2 * MAX_LAUNCH_LINE_BYTES);
    assert!(output.captured.len() <= MAX_LAUNCH_STDERR_BYTES);
    assert!(output.captured.capacity() <= 2 * MAX_LAUNCH_STDERR_BYTES);
}

#[test]
fn launch_stderr_keeps_bounded_buffers_for_any_line_length_and_chunking() {
    let noise = vec![b'x'; 1024 * 1024];
    for chunk in [1, 7, LAUNCH_STDERR_CHUNK, noise.len()] {
        let mut output = LaunchStderr::default();
        for piece in noise.chunks(chunk) {
            assert_eq!(output.push(piece), None);
            assert_bounded(&output);
        }
        assert_eq!(output.captured, noise[..MAX_LAUNCH_STDERR_BYTES]);
        assert_eq!(output.finish_line(), None);
    }
}

#[test]
fn launch_stderr_finds_address_split_across_chunks_after_long_noise() {
    let mut stream = vec![b'x'; 256 * 1024];
    stream.extend_from_slice(b"\n");
    stream.extend_from_slice(format!("DevTools listening on {ADDRESS}\n").as_bytes());
    for chunk in [1, 5, LAUNCH_STDERR_CHUNK] {
        let mut output = LaunchStderr::default();
        let found = stream.chunks(chunk).find_map(|piece| {
            let found = output.push(piece);
            assert_bounded(&output);
            found
        });
        assert_eq!(found.as_deref(), Some(ADDRESS), "нарезка {chunk}");
    }
    // Адрес в хвосте длинной строки без отдельного перевода строки и без
    // завершающего `\n` у EOF.
    let mut output = LaunchStderr::default();
    let mut tail = vec![b'x'; 64 * 1024];
    tail.extend_from_slice(format!("DevTools listening on {ADDRESS}").as_bytes());
    for piece in tail.chunks(LAUNCH_STDERR_CHUNK) {
        assert_eq!(output.push(piece), None);
        assert_bounded(&output);
    }
    assert_eq!(output.finish_line().as_deref(), Some(ADDRESS));
}

fn fake_proc(workspace: &TempWorkspace, own_pid: u32, processes: &[(u32, &str)]) -> PathBuf {
    let root = workspace.path().join("proc");
    fs::create_dir_all(&root).unwrap();
    std::os::unix::fs::symlink(own_pid.to_string(), root.join("self")).unwrap();
    for (pid, stat) in processes {
        let directory = root.join(pid.to_string());
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("stat"), stat).unwrap();
    }
    root
}

#[test]
fn zombie_descendants_do_not_keep_the_group_alive() {
    let workspace = TempWorkspace::create("browser-process-zombie-test").unwrap();
    // Поле comm с пробелом и скобкой не сдвигает разбор полей.
    let root = fake_proc(
        &workspace,
        1,
        &[
            (1, "1 (asset store) S 0 1 1 0 -1"),
            (41, "41 (chrome) (x)) Z 1 40 40 0 -1"),
            (42, "42 (chrome) X 1 40 40 0 -1"),
            (43, "43 (other) S 1 77 77 0 -1"),
        ],
    );
    assert!(!group_has_live_members(&root, 40, 1).unwrap());
    fs::write(root.join("41/stat"), "41 (chrome) (x)) S 1 40 40 0 -1").unwrap();
    assert!(group_has_live_members(&root, 40, 1).unwrap());
    // Недоказуемое `/proc` чужого пространства PID оставляет группу непустой.
    assert!(group_has_live_members(&root, 77, 2).unwrap());
    assert!(group_has_live_members(&workspace.path().join("absent"), 40, 1).unwrap());
    fs::write(root.join("41/stat"), "41 (chrome) (x)) Z 1 40 40 0 -1").unwrap();
    fs::write(root.join("43/stat"), "43 (other").unwrap();
    assert_eq!(
        group_has_live_members(&root, 40, 1).unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
    workspace.close().unwrap();
}

#[test]
fn live_group_is_seen_through_the_real_proc() {
    let group = getpgid(None).unwrap().as_raw_nonzero().get();
    assert!(group_has_live_members(Path::new(PROC_ROOT), group, std::process::id()).unwrap());
}
