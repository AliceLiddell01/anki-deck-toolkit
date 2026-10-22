use std::time::Duration;

use tokio::sync::mpsc;

use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Job {
    index: usize,
    generation: u32,
}

fn job(index: usize, generation: u32) -> Job {
    Job { index, generation }
}

fn ledger(
    jobs: Vec<Job>,
    workers: usize,
) -> (AssignmentLedger<Job>, Vec<mpsc::UnboundedReceiver<Job>>) {
    let (commands, receivers): (Vec<_>, Vec<_>) =
        (0..workers).map(|_| mpsc::unbounded_channel()).unzip();
    (AssignmentLedger::new(jobs, commands), receivers)
}

#[test]
fn dispatch_gives_one_job_per_ready_worker_and_out_of_order_completion_is_independent() {
    let (mut ledger, mut receivers) = ledger((0..6).map(|index| job(index, 1)).collect(), 4);
    // Не объявившие готовность исполнители заданий не получают.
    ledger.dispatch().unwrap();
    assert_eq!(ledger.in_flight(), 0);
    for worker in 1..=4 {
        ledger.mark_ready(worker).unwrap();
    }
    ledger.dispatch().unwrap();
    assert_eq!(ledger.in_flight(), 4);
    for (slot, receiver) in receivers.iter_mut().enumerate() {
        assert_eq!(receiver.try_recv().unwrap(), job(slot, 1));
        assert!(receiver.try_recv().is_err());
    }
    // Исполнитель 3 завершает раньше 1: порядок плана не влияет на приём.
    assert_eq!(
        ledger.accept(3, |assigned| assigned == &job(2, 1)).unwrap(),
        job(2, 1)
    );
    ledger.mark_ready(3).unwrap();
    ledger.dispatch().unwrap();
    assert_eq!(receivers[2].try_recv().unwrap(), job(4, 1));
    assert_eq!(
        ledger.accept(1, |assigned| assigned == &job(0, 1)).unwrap(),
        job(0, 1)
    );
    assert_eq!(ledger.active(1), None);
    assert_eq!(ledger.active(3), Some(&job(4, 1)));
}

#[test]
fn stale_or_foreign_result_is_rejected_and_keeps_assignment() {
    let (mut ledger, _receivers) = ledger(vec![job(0, 2), job(1, 1)], 2);
    ledger.mark_ready(1).unwrap();
    ledger.mark_ready(2).unwrap();
    ledger.dispatch().unwrap();
    // Устаревшее поколение того же элемента.
    assert_eq!(
        ledger.accept(1, |assigned| assigned == &job(0, 1)),
        Err(AssignmentError::Mismatch)
    );
    // Результат чужого назначения.
    assert_eq!(
        ledger.accept(2, |assigned| assigned == &job(0, 2)),
        Err(AssignmentError::Mismatch)
    );
    assert_eq!(ledger.in_flight(), 2);
    assert_eq!(
        ledger.accept(5, |_| true),
        Err(AssignmentError::UnknownWorker)
    );
    assert_eq!(
        ledger.accept(0, |_| true),
        Err(AssignmentError::UnknownWorker)
    );
    // Повтор принятого результата не засчитывается второй раз.
    ledger.accept(1, |assigned| assigned == &job(0, 2)).unwrap();
    assert_eq!(
        ledger.accept(1, |assigned| assigned == &job(0, 2)),
        Err(AssignmentError::NoAssignment)
    );
}

#[test]
fn requeued_job_goes_to_a_neighbour_when_worker_retires() {
    // В очереди только одно задание: соседу может достаться лишь возвращённое.
    let (mut ledger, mut receivers) = ledger(vec![job(0, 1)], 2);
    ledger.mark_ready(1).unwrap();
    ledger.dispatch().unwrap();
    let returned = ledger.accept(1, |_| true).unwrap();
    ledger.requeue(returned);
    assert!(ledger.retire(1).unwrap());
    assert!(!ledger.retire(1).unwrap());
    ledger.mark_ready(1).unwrap();
    ledger.mark_ready(2).unwrap();
    ledger.dispatch().unwrap();
    // Выведенный исполнитель больше ничего не получает, а его канал закрыт.
    assert_eq!(receivers[0].try_recv().unwrap(), job(0, 1));
    assert!(matches!(
        receivers[0].try_recv(),
        Err(mpsc::error::TryRecvError::Disconnected)
    ));
    assert_eq!(receivers[1].try_recv().unwrap(), job(0, 1));
    assert_eq!(ledger.active(2), Some(&job(0, 1)));
    assert_eq!(ledger.retired_workers(), vec![1]);
    assert_eq!(
        ledger.unfinished().cloned().collect::<Vec<_>>(),
        vec![job(0, 1)]
    );
}

#[test]
fn drained_pool_closes_idle_channels_only_after_last_active_job() {
    let (mut ledger, mut receivers) = ledger(vec![job(0, 1)], 2);
    ledger.mark_ready(1).unwrap();
    ledger.mark_ready(2).unwrap();
    ledger.dispatch().unwrap();
    // Сосед свободен, но активное задание ещё может вернуться в очередь.
    assert!(!ledger.close_if_drained());
    assert!(matches!(
        receivers[1].try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    ledger.accept(1, |_| true).unwrap();
    assert!(ledger.close_if_drained());
    assert_eq!(receivers[0].try_recv().unwrap(), job(0, 1));
    for receiver in &mut receivers {
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::error::TryRecvError::Disconnected)
        ));
    }
}

#[test]
fn stopped_dispatch_keeps_unissued_tail_and_active_jobs_unfinished() {
    let (mut ledger, _receivers) = ledger((0..5).map(|index| job(index, 1)).collect(), 2);
    ledger.mark_ready(1).unwrap();
    ledger.mark_ready(2).unwrap();
    ledger.dispatch().unwrap();
    ledger.stop_dispatch();
    ledger.accept(2, |_| true).unwrap();
    ledger.mark_ready(2).unwrap();
    ledger.dispatch().unwrap();
    assert_eq!(ledger.in_flight(), 1);
    assert_eq!(
        ledger.unfinished().map(|job| job.index).collect::<Vec<_>>(),
        vec![2, 3, 4, 0]
    );
}

#[test]
fn closed_worker_channel_is_reported_and_job_is_not_lost() {
    let (mut ledger, mut receivers) = ledger(vec![job(0, 1)], 1);
    receivers.remove(0);
    ledger.mark_ready(1).unwrap();
    assert_eq!(ledger.dispatch(), Err(AssignmentError::WorkerUnavailable));
    assert_eq!(ledger.in_flight(), 0);
    assert!(ledger.has_pending());
}

#[tokio::test]
async fn cancellation_keeps_first_reason_and_reaches_every_listener() {
    let cancellation = PoolCancellation::default();
    let mut first = cancellation.subscribe();
    let mut second = first.clone();
    assert_eq!(first.reason(), None);
    assert!(cancellation.request(CancellationReason::Interrupted));
    assert!(!cancellation.request(CancellationReason::CoordinatorFailure));
    assert_eq!(cancellation.reason(), Some(CancellationReason::Interrupted));
    assert_eq!(first.cancelled().await, CancellationReason::Interrupted);
    assert_eq!(second.cancelled().await, CancellationReason::Interrupted);
    // Подписка после отмены видит уже зафиксированную причину.
    assert_eq!(
        cancellation.subscribe().cancelled().await,
        CancellationReason::Interrupted
    );
}

#[tokio::test]
async fn dropped_coordinator_is_reported_as_coordinator_failure() {
    let cancellation = PoolCancellation::default();
    let mut listener = cancellation.subscribe();
    let waiting = tokio::spawn(async move { listener.cancelled().await });
    tokio::time::sleep(Duration::from_millis(10)).await;
    drop(cancellation);
    assert_eq!(
        waiting.await.unwrap(),
        CancellationReason::CoordinatorFailure
    );
}

#[tokio::test]
async fn signal_reaches_active_runs_and_exits_once_none_remain() {
    let dispatch = SignalDispatch::default();
    // Без запуска получения сигнал завершает процесс стандартным кодом.
    assert_eq!(
        dispatch.deliver(TerminationSignal::Terminate),
        SignalDelivery::Exit(143)
    );
    dispatch.enter();
    let first = tokio::spawn(dispatch.wait());
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert!(!first.is_finished());
    assert_eq!(
        dispatch.deliver(TerminationSignal::Interrupt),
        SignalDelivery::Delivered
    );
    first.await.unwrap().unwrap();
    // Пока запуск закрывает браузеры, повторный сигнал не обрывает остановку.
    assert_eq!(
        dispatch.deliver(TerminationSignal::Interrupt),
        SignalDelivery::Delivered
    );
    // Ожидание, начатое позже, не видит уже принятых сигналов.
    let later = tokio::spawn(dispatch.wait());
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert!(!later.is_finished());
    dispatch.leave();
    assert_eq!(
        dispatch.deliver(TerminationSignal::Hangup),
        SignalDelivery::Exit(129)
    );
    assert_eq!(
        dispatch.deliver(TerminationSignal::Interrupt),
        SignalDelivery::Exit(130)
    );
    later.abort();
}

#[test]
fn recovery_budget_counts_only_consecutive_failures() {
    let mut budget = RecoveryBudget::new(3);
    assert_eq!(budget.record_failure(), 1);
    assert_eq!(budget.record_failure(), 2);
    budget.record_success();
    assert_eq!(budget.consecutive_failures(), 0);
    budget.record_failure();
    budget.record_failure();
    assert!(!budget.exhausted());
    budget.record_failure();
    assert!(budget.exhausted());
}

const SIGNAL_CHILD: &str = "ASSET_STORE_TERMINATION_SIGNAL_CHILD";

/// Дочерний процесс теста ниже: обычным прогоном ничего не делает.
#[test]
fn termination_signal_child_process() {
    let Ok(mode) = std::env::var(SIGNAL_CHILD) else {
        return;
    };
    let me = rustix::process::getpid();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    if mode == "active" {
        // Запуск получения жив: сигнал передаётся ему, а процесс продолжает работу.
        let (scope, signal) = TerminationScope::enter();
        rustix::process::kill_process(me, rustix::process::Signal::TERM).unwrap();
        runtime
            .block_on(async { tokio::time::timeout(Duration::from_secs(5), signal).await })
            .unwrap()
            .unwrap();
        drop(scope);
        std::process::exit(0);
    }
    // Запуск закрыт: следующий сигнал завершает процесс стандартным кодом, а
    // не поглощается оставшейся регистрацией обработчика.
    drop(TerminationScope::enter());
    rustix::process::kill_process(me, rustix::process::Signal::TERM).unwrap();
    std::thread::sleep(Duration::from_secs(5));
    std::process::exit(0);
}

#[test]
fn signal_after_last_run_terminates_the_process_with_shell_code() {
    for (mode, code) in [("active", 0), ("released", 143)] {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "acquisition_pool::tests::termination_signal_child_process",
                "--nocapture",
            ])
            .env(SIGNAL_CHILD, mode)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(code), "{mode}");
    }
}
