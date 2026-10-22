//! Общий владелец назначений пула исполнителей получения ресурсов.
//!
//! Модуль не знает ни о браузере, ни о предметной идентичности: задание — это
//! непрозрачное значение `J`, а совпадение результата с назначением проверяет
//! переданный предметным адаптером предикат (Unicode и поколение у Kanji,
//! токен CAS у pitch accent). Здесь сосредоточены переходы, которые у обоих
//! доменов совпадают: очередь невыданных заданий, не более одного задания на
//! исполнителя, готовность к следующему заданию, возврат задания в очередь без
//! попытки, вывод исполнителя из пула, остановка выдачи, закрытие каналов
//! после исчерпания работы, отмена с первой причиной, серия неудачных
//! восстановлений и сигналы завершения процесса. Порядок завершения
//! исполнителей не связан с порядком исходного плана: задание всегда
//! принимается только тем исполнителем, которому было выдано.

use std::collections::VecDeque;
use std::io;
use std::sync::{Mutex, OnceLock};

use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{mpsc, watch};

/// Нарушение протокола назначения; предметный адаптер превращает его в свою ошибку.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum AssignmentError {
    #[error("неизвестный исполнитель")]
    UnknownWorker,
    #[error("результат без назначения")]
    NoAssignment,
    #[error("результат не совпал с назначением")]
    Mismatch,
    #[error("исполнитель не принимает назначение")]
    WorkerUnavailable,
}

/// Журнал назначений координатора. Номера исполнителей начинаются с `1`.
#[derive(Debug)]
pub(crate) struct AssignmentLedger<J> {
    pending: VecDeque<J>,
    active: Vec<Option<J>>,
    ready: Vec<bool>,
    retired: Vec<bool>,
    commands: Vec<Option<mpsc::UnboundedSender<J>>>,
}

impl<J: Clone> AssignmentLedger<J> {
    /// Исполнители изначально не готовы: готовность объявляет сам протокол.
    pub(crate) fn new(
        jobs: impl IntoIterator<Item = J>,
        commands: Vec<mpsc::UnboundedSender<J>>,
    ) -> Self {
        let workers = commands.len();
        Self {
            pending: jobs.into_iter().collect(),
            active: vec![None; workers],
            ready: vec![false; workers],
            retired: vec![false; workers],
            commands: commands.into_iter().map(Some).collect(),
        }
    }

    fn slot(&self, worker: usize) -> Result<usize, AssignmentError> {
        worker
            .checked_sub(1)
            .filter(|slot| *slot < self.active.len())
            .ok_or(AssignmentError::UnknownWorker)
    }

    pub(crate) fn mark_ready(&mut self, worker: usize) -> Result<(), AssignmentError> {
        let slot = self.slot(worker)?;
        self.ready[slot] = true;
        Ok(())
    }

    /// Выдаёт ожидающие задания готовым свободным исполнителям в порядке номеров.
    pub(crate) fn dispatch(&mut self) -> Result<(), AssignmentError> {
        for slot in 0..self.active.len() {
            if self.retired[slot] || !self.ready[slot] || self.active[slot].is_some() {
                continue;
            }
            let Some(command) = self.commands[slot].as_ref() else {
                continue;
            };
            let Some(job) = self.pending.pop_front() else {
                break;
            };
            if command.send(job.clone()).is_err() {
                self.pending.push_front(job);
                return Err(AssignmentError::WorkerUnavailable);
            }
            self.active[slot] = Some(job);
            self.ready[slot] = false;
        }
        self.close_if_drained();
        Ok(())
    }

    /// Принимает результат только от исполнителя, которому выдано именно это задание.
    /// При несовпадении назначение остаётся активным: результат не засчитывается.
    pub(crate) fn accept(
        &mut self,
        worker: usize,
        matches: impl FnOnce(&J) -> bool,
    ) -> Result<J, AssignmentError> {
        let slot = self.slot(worker)?;
        let assigned = self.active[slot]
            .as_ref()
            .ok_or(AssignmentError::NoAssignment)?;
        if !matches(assigned) {
            return Err(AssignmentError::Mismatch);
        }
        Ok(self.active[slot].take().expect("назначение проверено"))
    }

    /// Снимает назначение без результата (отмена или остановка исполнителя).
    /// Само задание не возвращается в очередь: решение принимает адаптер.
    pub(crate) fn release(&mut self, worker: usize) -> Result<Option<J>, AssignmentError> {
        let slot = self.slot(worker)?;
        Ok(self.active[slot].take())
    }

    /// Возвращает задание без терминального результата в конец очереди.
    pub(crate) fn requeue(&mut self, job: J) {
        self.pending.push_back(job);
    }

    /// Выводит исполнителя из пула; возвращает `true` при первом выводе.
    pub(crate) fn retire(&mut self, worker: usize) -> Result<bool, AssignmentError> {
        let slot = self.slot(worker)?;
        self.commands[slot].take();
        self.ready[slot] = false;
        Ok(!std::mem::replace(&mut self.retired[slot], true))
    }

    pub(crate) fn retired_workers(&self) -> Vec<usize> {
        (0..self.retired.len())
            .filter(|slot| self.retired[*slot])
            .map(|slot| slot + 1)
            .collect()
    }

    /// Прекращает выдачу: каналы закрываются, активные задания продолжаются.
    pub(crate) fn stop_dispatch(&mut self) {
        for command in &mut self.commands {
            command.take();
        }
    }

    /// Пока выполняется хотя бы одно задание, свободные исполнители остаются в
    /// пуле: отказ соседа может вернуть его задание в очередь.
    pub(crate) fn close_if_drained(&mut self) -> bool {
        let drained = self.pending.is_empty() && self.active.iter().all(Option::is_none);
        if drained {
            self.stop_dispatch();
        }
        drained
    }

    pub(crate) fn in_flight(&self) -> usize {
        self.active.iter().flatten().count()
    }

    pub(crate) fn active(&self, worker: usize) -> Option<&J> {
        self.slot(worker)
            .ok()
            .and_then(|slot| self.active[slot].as_ref())
    }

    /// Активные задания с номерами исполнителей.
    pub(crate) fn active_jobs(&self) -> impl Iterator<Item = (usize, &J)> {
        self.active
            .iter()
            .enumerate()
            .filter_map(|(slot, job)| job.as_ref().map(|job| (slot + 1, job)))
    }

    pub(crate) fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Невыданные и не принятые задания: после остановки они остаются в
    /// сохранённом состоянии ожидающими, без синтетического исхода.
    pub(crate) fn unfinished(&self) -> impl Iterator<Item = &J> {
        self.pending.iter().chain(self.active.iter().flatten())
    }
}

/// Причина отмены пула. Первая причина окончательна.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CancellationReason {
    /// Сигнал завершения процесса, полученный координатором.
    Interrupted,
    /// Отказ координатора или другого исполнителя.
    CoordinatorFailure,
}

impl CancellationReason {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Interrupted => "ctrl_c",
            Self::CoordinatorFailure => "coordinator_failure",
        }
    }
}

/// Отмена всех исполнителей пула с первой зафиксированной причиной.
#[derive(Debug)]
pub(crate) struct PoolCancellation {
    sender: watch::Sender<Option<CancellationReason>>,
}

impl Default for PoolCancellation {
    fn default() -> Self {
        Self {
            sender: watch::channel(None).0,
        }
    }
}

impl PoolCancellation {
    /// Возвращает `true`, если именно этот вызов зафиксировал причину.
    pub(crate) fn request(&self, reason: CancellationReason) -> bool {
        self.sender.send_if_modified(|current| {
            if current.is_some() {
                false
            } else {
                *current = Some(reason);
                true
            }
        })
    }

    #[cfg(test)]
    pub(crate) fn reason(&self) -> Option<CancellationReason> {
        *self.sender.borrow()
    }

    pub(crate) fn subscribe(&self) -> CancellationListener {
        CancellationListener {
            receiver: self.sender.subscribe(),
        }
    }
}

/// Наблюдатель отмены у исполнителя.
#[derive(Debug, Clone)]
pub(crate) struct CancellationListener {
    receiver: watch::Receiver<Option<CancellationReason>>,
}

impl CancellationListener {
    pub(crate) fn reason(&self) -> Option<CancellationReason> {
        *self.receiver.borrow()
    }

    /// Завершается при отмене. Исчезновение координатора считается его отказом.
    pub(crate) async fn cancelled(&mut self) -> CancellationReason {
        loop {
            if let Some(reason) = *self.receiver.borrow_and_update() {
                return reason;
            }
            if self.receiver.changed().await.is_err() {
                return self
                    .reason()
                    .unwrap_or(CancellationReason::CoordinatorFailure);
            }
        }
    }
}

/// Серия восстановлений сессии без доказанного здорового результата.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RecoveryBudget {
    consecutive_failures: u32,
    budget: u32,
}

impl RecoveryBudget {
    pub(crate) const fn new(budget: u32) -> Self {
        Self {
            consecutive_failures: 0,
            budget,
        }
    }

    /// Здоровый результат прерывает серию.
    pub(crate) fn record_success(&mut self) {
        self.consecutive_failures = 0;
    }

    /// Возвращает длину серии после очередного сбоя.
    pub(crate) fn record_failure(&mut self) -> u32 {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        self.consecutive_failures
    }

    pub(crate) fn consecutive_failures(&self) -> u32 {
        self.consecutive_failures
    }

    pub(crate) fn exhausted(&self) -> bool {
        self.consecutive_failures >= self.budget
    }
}

/// Сигнал завершения процесса, который получение обрабатывает как `Ctrl+C`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TerminationSignal {
    Interrupt,
    Terminate,
    Hangup,
}

impl TerminationSignal {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Interrupt => "SIGINT",
            Self::Terminate => "SIGTERM",
            Self::Hangup => "SIGHUP",
        }
    }

    /// Код выхода, которым оболочки обозначают завершение по этому сигналу.
    pub(crate) fn exit_code(self) -> i32 {
        128 + match self {
            Self::Interrupt => 2,
            Self::Terminate => 15,
            Self::Hangup => 1,
        }
    }
}

/// Слушатель сигналов завершения. Chromium запускается в собственной группе
/// процессов и не получает терминальные `SIGINT`/`SIGHUP`, поэтому их обязан
/// обработать CLI: прекратить выдачу и закрыть принадлежащие ему браузеры.
/// Как и `tokio::signal::ctrl_c`, регистрация заменяет стандартный обработчик
/// до конца процесса.
pub(crate) struct TerminationSignals {
    interrupt: tokio::signal::unix::Signal,
    terminate: tokio::signal::unix::Signal,
    hangup: tokio::signal::unix::Signal,
}

impl TerminationSignals {
    pub(crate) fn register() -> io::Result<Self> {
        Ok(Self {
            interrupt: signal(SignalKind::interrupt())?,
            terminate: signal(SignalKind::terminate())?,
            hangup: signal(SignalKind::hangup())?,
        })
    }

    pub(crate) async fn recv(&mut self) -> io::Result<TerminationSignal> {
        let received = tokio::select! {
            signal = self.interrupt.recv() => signal.map(|()| TerminationSignal::Interrupt),
            signal = self.terminate.recv() => signal.map(|()| TerminationSignal::Terminate),
            signal = self.hangup.recv() => signal.map(|()| TerminationSignal::Hangup),
        };
        let signal =
            received.ok_or_else(|| io::Error::other("поток сигналов завершения закрыт"))?;
        tracing::warn!(
            stage = "termination_signal",
            code = "termination_signal_received",
            signal = signal.name(),
            "получен сигнал завершения процесса"
        );
        Ok(signal)
    }
}

/// Что делать с сигналом, пришедшим в процессный обработчик.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SignalDelivery {
    /// Сигнал передан запускам получения: их остановка кооперативна.
    Delivered,
    /// Ни один запуск не владеет браузерами: стандартное завершение процесса.
    Exit(i32),
}

/// Учёт активных запусков получения и порядковый номер принятых сигналов.
#[derive(Debug)]
pub(crate) struct SignalDispatch {
    scopes: Mutex<usize>,
    received: watch::Sender<u64>,
}

impl Default for SignalDispatch {
    fn default() -> Self {
        Self {
            scopes: Mutex::new(0),
            received: watch::channel(0).0,
        }
    }
}

impl SignalDispatch {
    /// Решение и передача происходят под той же блокировкой, что и учёт
    /// запусков: сигнал после выхода последнего запуска завершает процесс.
    pub(crate) fn deliver(&self, signal: TerminationSignal) -> SignalDelivery {
        let scopes = self.scopes();
        if *scopes == 0 {
            return SignalDelivery::Exit(signal.exit_code());
        }
        self.received.send_modify(|received| *received += 1);
        SignalDelivery::Delivered
    }

    fn scopes(&self) -> std::sync::MutexGuard<'_, usize> {
        self.scopes
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    fn enter(&self) {
        *self.scopes() += 1;
    }

    fn leave(&self) {
        let mut scopes = self.scopes();
        *scopes = scopes.saturating_sub(1);
    }

    /// Ожидание сигнала, принятого после этого вызова.
    pub(crate) fn wait(&self) -> impl Future<Output = io::Result<()>> + Send + 'static {
        let mut receiver = self.received.subscribe();
        let baseline = *receiver.borrow_and_update();
        async move {
            receiver
                .wait_for(|received| *received > baseline)
                .await
                .map(|_| ())
                .map_err(|_| io::Error::other("обработчик сигналов завершения остановлен"))
        }
    }
}

/// Процессный владелец сигналов завершения. Регистрация tokio заменяет
/// стандартный обработчик до конца процесса, поэтому слушатель живёт в
/// собственном потоке, а не в среде выполнения отдельного запуска: сигнал,
/// который не ждёт ни один запуск получения, завершает процесс с кодом
/// 128 + номер сигнала, как стандартное поведение.
fn process_signals() -> io::Result<&'static SignalDispatch> {
    static PROCESS: OnceLock<Result<SignalDispatch, String>> = OnceLock::new();
    PROCESS
        .get_or_init(|| {
            let (registered, registration) = std::sync::mpsc::channel();
            std::thread::Builder::new()
                .name("termination-signals".into())
                .spawn(move || {
                    let runtime = match tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                    {
                        Ok(runtime) => runtime,
                        Err(error) => {
                            let _ = registered.send(Err(error));
                            return;
                        }
                    };
                    runtime.block_on(async move {
                        let mut signals = match TerminationSignals::register() {
                            Ok(signals) => {
                                let _ = registered.send(Ok(()));
                                signals
                            }
                            Err(error) => {
                                let _ = registered.send(Err(error));
                                return;
                            }
                        };
                        while let Ok(signal) = signals.recv().await {
                            let dispatch = process_signals()
                                .expect("обработчик зарегистрирован до первого сигнала");
                            if let SignalDelivery::Exit(code) = dispatch.deliver(signal) {
                                std::process::exit(code);
                            }
                        }
                    });
                })
                .map_err(|error| error.to_string())?;
            match registration.recv() {
                Ok(Ok(())) => Ok(SignalDispatch::default()),
                Ok(Err(error)) => Err(error.to_string()),
                Err(_) => Err("поток сигналов завершения не запустился".into()),
            }
        })
        .as_ref()
        .map_err(|error| io::Error::other(error.clone()))
}

/// Время жизни запуска получения, владеющего браузерами: пока область жива,
/// сигналы завершения только передаются его координатору.
#[derive(Debug)]
pub(crate) struct TerminationScope(Option<&'static SignalDispatch>);

impl TerminationScope {
    /// Регистрирует обработчик (при первом вызове) и возвращает область вместе
    /// с ожиданием первого сигнала (аналог `tokio::signal::ctrl_c`).
    pub(crate) fn enter() -> (Self, impl Future<Output = io::Result<()>> + Send + 'static) {
        match process_signals() {
            Ok(dispatch) => {
                dispatch.enter();
                (
                    Self(Some(dispatch)),
                    futures::future::Either::Left(dispatch.wait()),
                )
            }
            Err(error) => (
                Self(None),
                futures::future::Either::Right(std::future::ready(Err(error))),
            ),
        }
    }
}

impl Drop for TerminationScope {
    fn drop(&mut self) {
        if let Some(dispatch) = self.0 {
            dispatch.leave();
        }
    }
}

#[cfg(test)]
#[path = "acquisition_pool_tests.rs"]
mod tests;
