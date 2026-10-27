//! Warm scanner ownership, admission, deadlines and blocking execution.
use crate::{
    config::Config,
    errors::{fault, scan_fault},
    request::{MAX_REQUEST_STAMPS, RequestChecks, request_is_checked},
    scan::{Inspection, inspect_input_controlled, preflight},
};
use patronus_ark_core::{ScanControl, ScanError, Scanner};
use respan_gateway_conversation::Input;
use respan_gateway_core::Fault;
use std::{
    collections::HashSet,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::sync::Semaphore;

pub(super) struct CancelOnDrop(pub(super) ScanControl);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}
pub(super) struct State {
    pub(super) identity: u64,
    #[cfg(test)]
    pub(super) scans: std::sync::atomic::AtomicUsize,
    #[cfg(all(test, feature = "wolf"))]
    pub(super) model_documents: std::sync::atomic::AtomicUsize,
    pub(super) scanner: Mutex<Scanner>,
    pub(super) config: Config,
    pub(super) slots: Arc<Semaphore>,
    pub(super) active: Arc<Semaphore>,
}
#[derive(Clone)]
pub struct Ark(pub(super) Arc<State>);
#[derive(Clone)]
pub struct ProviderGuard(pub(super) Ark);
impl Ark {
    pub fn new(config: Config) -> Result<Self, Fault> {
        if config.max_queued > 1024
            || config.queue_timeout_ms == 0
            || config.scan_timeout_ms == 0
            || config.max_request_bytes == 0
        {
            return Err(Fault::config(
                "ark: positive limits and at most 1024 queued requests required",
            ));
        }
        let scanner = Scanner::warmup(config.scanner.clone(), config.warmup.clone())
            .map_err(|e| Fault::config(format!("ark: {e}")))?;
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Ok(Self(Arc::new(State {
            identity: NEXT.fetch_add(1, Ordering::Relaxed),
            #[cfg(test)]
            scans: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(all(test, feature = "wolf"))]
            model_documents: std::sync::atomic::AtomicUsize::new(0),
            scanner: Mutex::new(scanner),
            slots: Arc::new(Semaphore::new(config.max_queued + 1)),
            active: Arc::new(Semaphore::new(1)),
            config,
        })))
    }
    pub fn provider_guard(&self) -> ProviderGuard {
        ProviderGuard(self.clone())
    }
    #[cfg(test)]
    pub(super) async fn inspect(&self, input: Input) -> Result<Input, Fault> {
        self.inspect_request(input, RequestChecks::default()).await
    }
    pub(super) async fn inspect_request(
        &self,
        input: Input,
        ledger: RequestChecks,
    ) -> Result<Input, Fault> {
        preflight(&input, &self.0.config)?;
        if request_is_checked(&input, &self.0, &ledger)? {
            return Ok(input);
        }
        let state = Arc::clone(&self.0);
        let slot = Arc::clone(&state.slots)
            .try_acquire_owned()
            .map_err(|_| fault("ark_overloaded", "Ark scan queue is full"))?;
        let active = tokio::time::timeout(
            Duration::from_millis(state.config.queue_timeout_ms),
            Arc::clone(&state.active).acquire_owned(),
        )
        .await
        .map_err(|_| fault("ark_queue_timeout", "Ark queue deadline exceeded"))?
        .map_err(|_| Fault::internal("Ark scanner closed"))?;
        let timeout = Duration::from_millis(state.config.scan_timeout_ms);
        let control = ScanControl::with_timeout(timeout).map_err(scan_fault)?;
        let cancellation = CancelOnDrop(control.clone());
        let job_control = control.clone();
        let job_ledger = ledger.clone();
        let task = tokio::task::spawn_blocking(move || {
            // These permits live until the real job ends, including after cancellation/timeout.
            let (_slot, _active) = (slot, active);
            let mut scanner = state
                .scanner
                .lock()
                .map_err(|_| Fault::internal("Ark scanner poisoned"))?;
            job_control.check().map_err(scan_fault)?;
            let mut inspection = Inspection {
                scanner: &mut scanner,
                config: &state.config,
                control: &job_control,
                ledger: &job_ledger,
                identity: state.identity,
                pending: HashSet::new(),
                #[cfg(test)]
                scans: Some(&state.scans),
                #[cfg(all(test, feature = "wolf"))]
                model_documents: Some(&state.model_documents),
            };
            let output = inspect_input_controlled(input, &mut inspection)?;
            job_control.check().map_err(scan_fault)?;
            // Keep capacity until the awaiting hook accepts the completed work.
            // A timed-out/dropped hook cannot publish these pending stamps.
            Ok::<_, Fault>((output, inspection.pending, _slot, _active))
        });
        let result = match tokio::time::timeout(timeout, task).await {
            Ok(result) => {
                let (output, pending, _slot, _active) =
                    result.map_err(|_| Fault::internal("Ark scanner task failed"))??;
                control.check().map_err(scan_fault)?;
                let mut checked = ledger
                    .0
                    .lock()
                    .map_err(|_| Fault::internal("Ark request checks poisoned"))?;
                for stamp in pending {
                    if checked.len() < MAX_REQUEST_STAMPS {
                        checked.insert(stamp);
                    }
                }
                Ok(output)
            }
            Err(_) => {
                control.cancel_deadline();
                Err(scan_fault(ScanError::DeadlineExceeded))
            }
        };
        drop(cancellation);
        result
    }
}
impl std::fmt::Debug for Ark {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ark").finish_non_exhaustive()
    }
}
impl std::fmt::Debug for ProviderGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderGuard").finish_non_exhaustive()
    }
}
