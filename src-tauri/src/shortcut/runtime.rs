//! Production command orchestration and shortcut mutation coordination.

use super::ImplementationChangeResult;
use super::{
    handy_keys,
    switch::{self, NativeFailure, NativeOperations, Registration, Role},
    tauri_impl,
};
use crate::settings;
use crate::settings::KeyboardImplementation;
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Manager};

#[derive(Default)]
struct AdmissionState {
    busy: bool,
    worker: bool,
    generation: u64,
    drained: u64,
    cancel: bool,
    capture: bool,
    degraded: Option<String>,
}

impl AdmissionState {
    // Reserve the worker under the same short lock as requests and permit release.
    // A callback either joins the running generation or owns the next wakeup.
    fn schedule(&mut self) -> bool {
        if !self.busy && !self.worker && self.generation != self.drained {
            self.worker = true;
            true
        } else {
            false
        }
    }
    fn enter(&mut self) -> Result<(), String> {
        self.enter_operation(false)
    }
    fn enter_operation(&mut self, cleanup: bool) -> Result<(), String> {
        if self.busy || self.worker {
            return Err(
                "Keyboard shortcuts are busy; retry after the current operation finishes".into(),
            );
        }
        if !cleanup {
            if let Some(error) = &self.degraded {
                return Err(format!(
                    "Keyboard runtime is degraded; restart required: {error}"
                ));
            }
        }
        self.busy = true;
        Ok(())
    }
    fn request(&mut self, cancel: Option<bool>) -> bool {
        if let Some(cancel) = cancel {
            self.cancel = cancel;
        }
        self.generation = self.generation.wrapping_add(1);
        self.schedule()
    }
    fn release(&mut self, generation: Option<u64>) -> bool {
        self.busy = false;
        if let Some(generation) = generation {
            self.drained = generation;
            self.worker = false;
        }
        self.schedule()
    }
}

#[derive(Default)]
struct Admission(Mutex<AdmissionState>);

fn admission(app: &AppHandle) -> Arc<Admission> {
    if app.try_state::<Arc<Admission>>().is_none() {
        app.manage(Arc::new(Admission::default()));
    }
    app.state::<Arc<Admission>>().inner().clone()
}

pub(crate) struct Permit {
    app: AppHandle,
    admission: Arc<Admission>,
    generation: Option<u64>,
}
impl Drop for Permit {
    fn drop(&mut self) {
        let schedule = self
            .admission
            .0
            .lock()
            .map(|mut state| state.release(self.generation))
            .unwrap_or(false);
        if schedule {
            spawn_reconciliation(self.app.clone(), self.admission.clone());
        }
    }
}

pub(crate) fn admit(app: &AppHandle) -> Result<Permit, String> {
    let admission = admission(app);
    admission
        .0
        .lock()
        .map_err(|_| "Keyboard operation state poisoned")?
        .enter()?;
    Ok(Permit {
        app: app.clone(),
        admission,
        generation: None,
    })
}

/// Native capture must remain terminable even when restoration is degraded.
/// This does not bypass serialization or permit new native registrations.
pub(crate) fn admit_cleanup(app: &AppHandle) -> Result<Permit, String> {
    let admission = admission(app);
    admission.0.lock().map_err(|_| "Keyboard operation state poisoned")?.enter_operation(true)?;
    Ok(Permit { app: app.clone(), admission, generation: None })
}

pub(crate) fn request_reconciliation(app: &AppHandle, cancel: Option<bool>) {
    let admission = admission(app);
    let schedule = admission
        .0
        .lock()
        .map(|mut state| state.request(cancel))
        .unwrap_or(false);
    if schedule {
        spawn_reconciliation(app.clone(), admission);
    }
}

fn spawn_reconciliation(app: AppHandle, admission: Arc<Admission>) {
    tauri::async_runtime::spawn_blocking(move || {
        let generation = match admission.0.lock() {
            Ok(mut state) => {
                state.busy = true;
                state.generation
            }
            Err(_) => return,
        };
        let _permit = Permit {
            app: app.clone(),
            admission,
            generation: Some(generation),
        };
        crate::secure_input::reconcile_fallback_admitted(&app);
    });
}

pub(crate) fn intent(app: &AppHandle) -> Result<(bool, bool), String> {
    let admission = admission(app);
    let state = admission
        .0
        .lock()
        .map_err(|_| "Keyboard operation state poisoned")?;
    if let Some(error) = &state.degraded {
        return Err(format!("Keyboard runtime degraded: {error}"));
    }
    Ok((state.cancel, state.capture))
}

fn mark_degraded(app: &AppHandle, message: String) {
    if let Ok(mut state) = admission(app).0.lock() {
        state.degraded = Some(message);
    }
}

pub(crate) fn snapshot(app: &AppHandle) -> Result<Vec<Registration>, String> {
    let mut owned = tauri_impl::snapshot(app)?;
    owned.extend(handy_keys::snapshot(app)?);
    switch::fold_roles(owned)
}

pub(crate) struct Native<'a>(pub &'a AppHandle);
impl NativeOperations for Native<'_> {
    fn initialize(&mut self, backend: KeyboardImplementation) -> Result<(), NativeFailure> {
        if backend == KeyboardImplementation::HandyKeys {
            handy_keys::ensure_initialized(self.0)?;
        }
        Ok(())
    }
    fn remove(&mut self, registration: &Registration) -> Result<(), NativeFailure> {
        match registration.backend {
            KeyboardImplementation::Tauri => {
                tauri_impl::unregister_report(self.0, registration.binding.clone())
            }
            KeyboardImplementation::HandyKeys => self
                .0
                .try_state::<handy_keys::HandyKeysState>()
                .ok_or("HandyKeys is not initialized")?
                .unregister_report(&registration.binding),
        }
    }
    fn install(&mut self, registration: &Registration) -> Result<(), NativeFailure> {
        match registration.backend {
            KeyboardImplementation::Tauri => {
                tauri_impl::register_report(self.0, registration.binding.clone())
            }
            KeyboardImplementation::HandyKeys => self
                .0
                .try_state::<handy_keys::HandyKeysState>()
                .ok_or("HandyKeys is not initialized")?
                .register_report(&registration.binding),
        }
    }
}

fn publish_report(app: &AppHandle, report: &switch::SwitchReport) -> Result<(), String> {
    if !report.applied() {
        let error = report.to_string();
        if !report.rollback_complete() {
            mark_degraded(app, error.clone());
        }
        return Err(error);
    }
    Ok(())
}

/// Compose the complete intended ownership before any native mutation. The
/// switch caller supplies prepared/reset primaries and the corresponding binding
/// map; no persisted backend is consulted here. Role folding retains Carbon
/// callbacks across primary/shadow transfers (including outstanding releases).
pub(crate) fn desired_native_set(
    intent: &crate::secure_input::reconciliation::Intent<'_>,
    primaries: Vec<Registration>,
) -> Result<(Vec<Registration>, crate::secure_input::reconciliation::Plan), String> {
    use crate::secure_input::reconciliation::{included, plan};
    let mut desired: Vec<_> = primaries
        .into_iter()
        .filter_map(|mut entry| {
            entry.roles.retain(|role| *role == Role::Primary);
            (!entry.roles.is_empty()
                && entry.backend == intent.backend
                && included(intent, &entry.binding.id))
            .then_some(entry)
        })
        .collect();
    if included(intent, "cancel") {
        if let Some(binding) = intent.bindings.get("cancel") {
            desired.push(Registration::new(
                intent.backend,
                binding.clone(),
                Role::Cancel,
            )?);
        }
    }
    let shadows = plan(intent);
    desired.extend(shadows.registrations.iter().cloned());
    Ok((switch::fold_roles(desired)?, shadows))
}

/// Already admitted native delta, reusable by switching. Required shadow work
/// is in this same report, not a best-effort follow-up after settings publication.
/// Return the full failure/rollback report to the transaction owner.
pub(crate) fn apply_native_delta(
    native: &mut impl NativeOperations,
    backend: KeyboardImplementation,
    previous: &[Registration],
    desired: &[Registration],
) -> switch::SwitchReport {
    switch::apply(native, backend, previous, desired)
}

fn apply_and_publish_delta(
    app: &AppHandle,
    backend: KeyboardImplementation,
    previous: &[Registration],
    desired: &[Registration],
    shadows: &crate::secure_input::reconciliation::Plan,
) -> Result<switch::SwitchReport, String> {
    let report = apply_native_delta(&mut Native(app), backend, previous, desired);
    if let Err(error) = tauri_impl::publish_roles(app, &report.owned) {
        mark_degraded(app, error.clone());
        return Err(error);
    }
    crate::secure_input::publish_coverage(
        app,
        crate::secure_input::reconciliation::coverage(shadows, &report.owned, &report.uncertain),
    );
    Ok(report)
}

pub(crate) fn reconcile_admitted(app: &AppHandle) -> Result<(), String> {
    let (cancel, captured) = intent(app)?;
    if app
        .try_state::<crate::commands::ShortcutsInitialized>()
        .is_none()
    {
        return Ok(());
    }
    let settings = settings::get_settings(app);
    let previous = snapshot(app)?;
    let explicit = crate::secure_input::reconciliation::Intent {
        backend: settings.keyboard_implementation,
        bindings: &settings.bindings,
        post_process_enabled: settings.post_process_enabled,
        ready: true,
        sustained: crate::secure_input::sustained(app),
        cancel,
        captured,
    };
    let (desired, shadows) = match desired_native_set(&explicit, previous.clone()) {
        Ok(desired) => desired,
        Err(error) => {
            // Planning rejection does not mutate ownership, but must still
            // expose missing coverage (for example colliding widened shadows).
            let shadows = crate::secure_input::reconciliation::plan(&explicit);
            crate::secure_input::publish_coverage(
                app,
                crate::secure_input::reconciliation::coverage(&shadows, &previous, &[]),
            );
            return Err(error);
        }
    };
    let report = apply_and_publish_delta(app, explicit.backend, &previous, &desired, &shadows)?;
    publish_report(app, &report)
}

pub(crate) fn capture_admitted(app: &AppHandle, suspended: bool) -> Result<(), String> {
    let (cancel, captured) = intent(app)?;
    if suspended && captured {
        return Err("Shortcut capture is already active; finish or cancel it first".into());
    }
    if app
        .try_state::<crate::commands::ShortcutsInitialized>()
        .is_none()
    {
        return Err("Shortcuts have not been initialized".into());
    }
    let current = settings::get_settings(app);
    let previous = snapshot(app)?;
    let (report, shadows) = capture_delta(
        &mut Native(app),
        &current,
        &previous,
        cancel,
        suspended,
        crate::secure_input::sustained(app),
    )?;
    if let Err(error) = tauri_impl::publish_roles(app, &report.owned) {
        let error = format!("Capture ownership publication failed: {error}; {report}");
        mark_degraded(app, error.clone());
        return Err(error);
    }
    crate::secure_input::publish_coverage(
        app,
        crate::secure_input::reconciliation::coverage(&shadows, &report.owned, &report.uncertain),
    );
    publish_report(app, &report).map_err(|error| {
        format!("Shortcut {} failed: {error}", if suspended { "suspension" } else { "restoration" })
    })?;
    admission(app)
        .0
        .lock()
        .map_err(|_| "Keyboard operation state poisoned")?
        .capture = suspended;
    request_reconciliation(app, None);
    Ok(())
}

/// The capture owner uses the switching task's native batch/compensation report.
/// Kept injectable so capture eligibility and every native failure are exercised
/// through the same planning and application path as the public commands.
fn capture_delta(
    native: &mut impl NativeOperations,
    current: &settings::AppSettings,
    previous: &[Registration],
    cancel: bool,
    suspended: bool,
    sustained: bool,
) -> Result<(switch::SwitchReport, crate::secure_input::reconciliation::Plan), String> {
    let primaries = if suspended {
        Vec::new()
    } else {
        let plan = switch::prepare(
            current.keyboard_implementation,
            &current.bindings,
            &settings::get_default_settings().bindings,
            current.post_process_enabled,
        )?;
        if !plan.resets.is_empty() {
            return Err(
                "Cannot resume incompatible bindings without an explicit backend switch".into(),
            );
        }
        plan.registrations
    };
    let explicit = crate::secure_input::reconciliation::Intent {
        backend: current.keyboard_implementation,
        bindings: &current.bindings,
        post_process_enabled: current.post_process_enabled,
        ready: true,
        sustained,
        cancel,
        captured: suspended,
    };
    let (desired, shadows) = desired_native_set(&explicit, primaries)?;
    let report = apply_native_delta(native, explicit.backend, previous, &desired);
    Ok((report, shadows))
}

pub(crate) struct Command<'a> {
    pub app: &'a AppHandle,
}
impl NativeOperations for Command<'_> {
    fn initialize(&mut self, backend: KeyboardImplementation) -> Result<(), NativeFailure> {
        Native(self.app).initialize(backend)
    }
    fn remove(&mut self, registration: &Registration) -> Result<(), NativeFailure> {
        Native(self.app).remove(registration)
    }
    fn install(&mut self, registration: &Registration) -> Result<(), NativeFailure> {
        Native(self.app).install(registration)
    }
}
impl CommandOperations for Command<'_> {
    fn stop_capture(&mut self) -> Result<(), String> {
        handy_keys::stop_recording_admitted(self.app)
    }
    fn settings(&self) -> settings::AppSettings {
        settings::get_settings(self.app)
    }
    fn snapshot(&self) -> Result<Vec<Registration>, String> {
        snapshot(self.app)
    }
    fn context(&self) -> Result<(bool, bool, bool, bool), String> {
        let (cancel, capture) = intent(self.app)?;
        Ok((
            cancel,
            capture,
            self.app
                .try_state::<crate::commands::ShortcutsInitialized>()
                .is_some(),
            crate::secure_input::sustained(self.app),
        ))
    }
    fn write(&mut self, current: settings::AppSettings) {
        settings::write_settings(self.app, current);
    }
    fn publish(
        &mut self,
        report: &switch::SwitchReport,
        shadows: &crate::secure_input::reconciliation::Plan,
    ) -> Result<(), String> {
        tauri_impl::publish_roles(self.app, &report.owned)?;
        crate::secure_input::publish_coverage(
            self.app,
            crate::secure_input::reconciliation::coverage(
                shadows,
                &report.owned,
                &report.uncertain,
            ),
        );
        Ok(())
    }
    fn degraded(&mut self, error: String) {
        mark_degraded(self.app, error);
    }
    fn emit(&mut self, resets: &[String]) {
        use tauri::Emitter;
        let _ = self.app.emit(
            "settings-changed",
            keyboard_implementation_event(
                settings::get_settings(self.app).keyboard_implementation,
                resets,
            ),
        );
    }
}

fn keyboard_implementation_event(
    backend: KeyboardImplementation,
    resets: &[String],
) -> serde_json::Value {
    serde_json::json!({
        "setting": "keyboard_implementation",
        "value": backend,
        "reset_bindings": resets,
    })
}

pub(crate) trait CommandOperations: NativeOperations {
    /// Terminate HandyKeys capture and discharge its suspension before ownership
    /// snapshots. A different editor's suspension must remain untouched.
    fn stop_capture(&mut self) -> Result<(), String>;
    fn settings(&self) -> settings::AppSettings;
    fn snapshot(&self) -> Result<Vec<Registration>, String>;
    fn context(&self) -> Result<(bool, bool, bool, bool), String>;
    fn write(&mut self, settings: settings::AppSettings);
    fn publish(
        &mut self,
        report: &switch::SwitchReport,
        shadows: &crate::secure_input::reconciliation::Plan,
    ) -> Result<(), String>;
    fn degraded(&mut self, error: String);
    fn emit(&mut self, resets: &[String]);
}

pub(super) fn same_keyboard_configuration(
    a: &settings::AppSettings,
    b: &settings::AppSettings,
) -> bool {
    a.keyboard_implementation == b.keyboard_implementation
        && a.post_process_enabled == b.post_process_enabled
        && serde_json::to_value(&a.bindings).ok() == serde_json::to_value(&b.bindings).ok()
}

/// IPC and injected tests share this owner. Caller holds admission before snapshots.
pub(crate) fn orchestrate(
    operations: &mut impl CommandOperations,
    backend: KeyboardImplementation,
) -> Result<ImplementationChangeResult, String> {
    operations.stop_capture()?;
    let original = operations.settings();
    let previous = operations
        .snapshot()
        .map_err(|error| format!("Snapshot {backend:?}: {error}"))?;
    let (cancel, captured, ready, sustained) = operations.context()?;
    if captured {
        return Err(
            "Cannot switch keyboard implementation while shortcut capture is active".into(),
        );
    }
    if !ready {
        return Err(format!(
            "Prepare {backend:?}: shortcuts have not been initialized"
        ));
    }
    let prepared = switch::prepare(
        backend,
        &original.bindings,
        &settings::get_default_settings().bindings,
        original.post_process_enabled,
    )
    .map_err(|error| format!("Prepare {backend:?}: {error}"))?;
    let mut candidate = original.clone();
    candidate.keyboard_implementation = backend;
    for binding in &prepared.resets {
        candidate
            .bindings
            .insert(binding.id.clone(), binding.clone());
    }
    let explicit = crate::secure_input::reconciliation::Intent {
        backend,
        bindings: &candidate.bindings,
        post_process_enabled: candidate.post_process_enabled,
        ready,
        sustained,
        cancel,
        captured,
    };
    let (desired, shadows) = desired_native_set(&explicit, prepared.registrations)
        .map_err(|error| format!("Prepare {backend:?}: {error}"))?;
    let old_shadows =
        crate::secure_input::reconciliation::plan(&crate::secure_input::reconciliation::Intent {
            backend: original.keyboard_implementation,
            bindings: &original.bindings,
            post_process_enabled: original.post_process_enabled,
            ready,
            sustained,
            cancel,
            captured,
        });
    let mut report = apply_native_delta(operations, backend, &previous, &desired);
    if report.applied() {
        let current = operations.settings();
        let rejection = if !same_keyboard_configuration(&original, &current) {
            Some("keyboard configuration changed during native application".to_string())
        } else {
            operations.publish(&report, &shadows).err()
        };
        if let Some(error) = rejection {
            report.initialization_error = Some(switch::OperationError {
                stage: switch::Stage::Commit,
                backend,
                registration: None,
                failure: NativeFailure::Rejected(error),
            });
            switch::rollback(operations, &mut report, &previous);
        } else {
            // Merge only transaction-owned fields into the current preferences.
            let mut merged = current;
            merged.keyboard_implementation = backend;
            for binding in &prepared.resets {
                merged.bindings.insert(binding.id.clone(), binding.clone());
            }
            operations.write(merged);
            let resets = prepared
                .resets
                .iter()
                .map(|binding| binding.id.clone())
                .collect::<Vec<_>>();
            operations.emit(&resets);
            return Ok(ImplementationChangeResult {
                success: true,
                reset_bindings: resets,
            });
        }
    }
    let mut error = report.to_string();
    if let Err(publication) = operations.publish(&report, &old_shadows) {
        error.push_str(&format!(
            "; Publish {backend:?}: {publication}; ownership metadata uncertain"
        ));
        operations.degraded(error.clone());
    }
    if !report.rollback_complete() {
        operations.degraded(error.clone());
    }
    Err(error)
}

#[cfg(test)]
mod capture_tests;

#[cfg(test)]
mod matrix_tests;

#[cfg(all(test, target_os = "windows"))]
mod windows_native_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admission_rejects_overlapping_and_reentrant_commands() {
        let mut state = AdmissionState::default();
        state.enter().unwrap();
        // Switch, binding, post-processing and capture share this admission.
        for _ in 0..4 {
            assert!(state.enter().unwrap_err().contains("busy"));
        }
        assert!(!state.request(Some(true)));
        assert!(!state.request(Some(false)));
        assert!(state.release(None));
        assert!(!state.cancel);
        assert!(
            state.enter().is_err(),
            "scheduled worker reserves admission"
        );
    }

    #[test]
    fn admission_drains_request_during_worker_native_callback() {
        let mut state = AdmissionState::default();
        assert!(state.request(Some(true)));
        state.busy = true;
        let generation = state.generation;
        assert!(!state.request(Some(false)));
        assert!(
            !state.request(None),
            "monitor requests coalesce with cancel"
        );
        assert!(state.release(Some(generation)));
        assert!(!state.cancel);
        state.busy = true;
        let generation = state.generation;
        assert!(!state.release(Some(generation)));
        state.enter().unwrap();
    }

    #[test]
    fn admission_request_after_final_release_schedules_new_worker() {
        let mut state = AdmissionState::default();
        assert!(state.request(Some(true)));
        state.busy = true;
        let generation = state.generation;
        assert!(!state.release(Some(generation)));
        assert!(state.request(Some(false)));
        assert!(!state.request(None));
    }

    #[test]
    fn admission_release_racing_lifecycle_always_has_exactly_one_wakeup() {
        use std::sync::Barrier;
        for worker_release in [false, true] {
            for _ in 0..32 {
                let state = Arc::new(Mutex::new(AdmissionState::default()));
                let generation = {
                    let mut state = state.lock().unwrap();
                    if worker_release {
                        assert!(state.request(Some(true)));
                        state.busy = true;
                        Some(state.generation)
                    } else {
                        state.enter().unwrap();
                        None
                    }
                };
                let barrier = Arc::new(Barrier::new(2));
                let request_state = state.clone();
                let request_barrier = barrier.clone();
                let request = std::thread::spawn(move || {
                    request_barrier.wait();
                    request_state.lock().unwrap().request(Some(false))
                });
                barrier.wait();
                let released = state.lock().unwrap().release(generation);
                let requested = request.join().unwrap();
                assert_ne!(released, requested, "one side must own the handoff");
                let state = state.lock().unwrap();
                assert!(state.worker);
                assert!(!state.cancel);
                assert_ne!(state.generation, state.drained);
            }
        }
    }

    #[test]
    fn admission_preserves_capture_and_rejects_degraded_mutations() {
        let mut state = AdmissionState::default();
        state.enter().unwrap();
        state.capture = true;
        assert!(!state.release(None));
        state.enter().unwrap();
        assert!(state.capture);
        state.degraded = Some("uncertain native ownership".into());
        state.release(None);
        assert!(state.enter().unwrap_err().contains("degraded"));
    }

    struct RejectedCommand {
        selected: KeyboardImplementation,
        current: settings::AppSettings,
        owned: Vec<Registration>,
        writes: usize,
        events: usize,
        reject: bool,
        conflict: bool,
        uncertain: bool,
        degraded: bool,
        calls: Vec<&'static str>,
        fail_ready: bool,
        sustained: bool,
        cancel: bool,
    }
    impl RejectedCommand {
        fn new() -> Self {
            let mut current = settings::get_default_settings();
            current.keyboard_implementation = KeyboardImplementation::Tauri;
            current.post_process_enabled = false;
            let owned = switch::prepare(
                current.keyboard_implementation,
                &current.bindings,
                &current.bindings,
                false,
            )
            .unwrap()
            .registrations;
            Self {
                selected: current.keyboard_implementation,
                current,
                owned,
                writes: 0,
                events: 0,
                reject: true,
                conflict: false,
                uncertain: false,
                degraded: false,
                calls: vec![],
                fail_ready: false,
                sustained: false,
                cancel: false,
            }
        }
    }
    impl NativeOperations for RejectedCommand {
        fn initialize(&mut self, _: KeyboardImplementation) -> Result<(), NativeFailure> {
            self.calls.push("initialize");
            if self.fail_ready {
                Err("readiness rejected".into())
            } else {
                Ok(())
            }
        }
        fn remove(&mut self, entry: &Registration) -> Result<(), NativeFailure> {
            self.calls.push("remove");
            self.owned.retain(|owner| !owner.same_native_owner(entry));
            Ok(())
        }
        fn install(&mut self, entry: &Registration) -> Result<(), NativeFailure> {
            self.calls.push("install");
            if entry.backend == KeyboardImplementation::HandyKeys && self.reject {
                return Err(if self.uncertain {
                    NativeFailure::Indeterminate("lost response".into())
                } else {
                    NativeFailure::Rejected("native registration conflict".into())
                });
            }
            self.owned.push(entry.clone());
            // Simulate another preference command during native work.
            self.current.audio_feedback = !self.current.audio_feedback;
            if self.conflict {
                self.current.post_process_enabled = true;
            }
            Ok(())
        }
    }
    impl CommandOperations for RejectedCommand {
        fn stop_capture(&mut self) -> Result<(), String> { Ok(()) }
        fn settings(&self) -> settings::AppSettings {
            self.current.clone()
        }
        fn snapshot(&self) -> Result<Vec<Registration>, String> {
            if self.degraded {
                Err("uncertain native ownership".into())
            } else {
                Ok(self.owned.clone())
            }
        }
        fn context(&self) -> Result<(bool, bool, bool, bool), String> {
            Ok((self.cancel, false, true, self.sustained))
        }
        fn write(&mut self, current: settings::AppSettings) {
            assert_eq!(
                current.audio_feedback, self.current.audio_feedback,
                "unrelated preference must survive"
            );
            self.selected = current.keyboard_implementation;
            self.current = current;
            self.writes += 1;
        }
        fn publish(
            &mut self,
            report: &switch::SwitchReport,
            _: &crate::secure_input::reconciliation::Plan,
        ) -> Result<(), String> {
            self.owned = report.owned.clone();
            Ok(())
        }
        fn degraded(&mut self, _: String) {
            self.degraded = true;
        }
        fn emit(&mut self, _: &[String]) {
            assert_eq!(self.writes, self.events + 1);
            self.events += 1;
        }
    }

    #[test]
    fn production_orchestration_rejects_without_settings_or_success_event() {
        let mut command = RejectedCommand::new();
        let previous = command.owned.clone();
        let result = orchestrate(&mut command, KeyboardImplementation::HandyKeys);
        assert_eq!(
            command.selected,
            KeyboardImplementation::Tauri,
            "native rejection must not publish backend settings"
        );
        assert_eq!(command.writes, 0);
        assert_eq!(command.events, 0);
        let error = result.err().unwrap();
        assert!(error.contains("previous configuration restored"));
        assert!(error.contains("InstallCandidate HandyKeys binding"));
        assert_eq!(command.calls.first(), Some(&"initialize"));
        assert_eq!(command.owned.len(), previous.len());
        assert!(previous.iter().all(|old| command
            .owned
            .iter()
            .any(|owner| owner.same_native_owner(old))));
    }

    #[test]
    fn production_orchestration_readiness_and_preparation_precede_removal() {
        let mut command = RejectedCommand::new();
        command.fail_ready = true;
        let error = orchestrate(&mut command, KeyboardImplementation::HandyKeys)
            .err()
            .unwrap();
        assert!(error.contains("Initialize HandyKeys"));
        assert_eq!(command.calls, vec!["initialize"]);
        assert_eq!((command.writes, command.events), (0, 0));
        command.calls.clear();
        let binding = command.current.bindings.get_mut("transcribe").unwrap();
        binding.id = "wrong callback".into();
        assert!(orchestrate(&mut command, KeyboardImplementation::HandyKeys)
            .err()
            .unwrap()
            .contains("Prepare HandyKeys"));
        assert!(command.calls.is_empty());
    }

    #[test]
    fn production_orchestration_keeps_shadow_owner_and_active_cancel() {
        let mut command = RejectedCommand::new();
        command.reject = false;
        command.sustained = true;
        command.cancel = true;
        // Use a Carbon-compatible primary so the transaction must retain its handle.
        command
            .current
            .bindings
            .get_mut("transcribe")
            .unwrap()
            .current_binding = "ctrl+shift+a".into();
        command.owned = switch::prepare(
            KeyboardImplementation::Tauri,
            &command.current.bindings,
            &command.current.bindings,
            false,
        )
        .unwrap()
        .registrations;
        let old = command
            .owned
            .iter()
            .find(|entry| entry.binding.id == "transcribe")
            .unwrap()
            .clone();
        orchestrate(&mut command, KeyboardImplementation::HandyKeys).unwrap();
        let shadow = command
            .owned
            .iter()
            .find(|entry| entry.same_native_owner(&old))
            .unwrap();
        assert_eq!(shadow.roles, vec![Role::Shadow]);
        assert_eq!(
            command
                .owned
                .iter()
                .any(|entry| entry.roles.contains(&Role::Cancel)),
            !cfg!(target_os = "linux")
        );
        orchestrate(&mut command, KeyboardImplementation::Tauri).unwrap();
        let primary = command
            .owned
            .iter()
            .find(|entry| entry.same_native_owner(&old))
            .unwrap();
        assert_eq!(primary.roles, vec![Role::Primary]);
        assert_eq!((command.writes, command.events), (2, 2));
    }

    #[test]
    fn production_orchestration_commit_conflict_rolls_back() {
        let mut command = RejectedCommand::new();
        command.reject = false;
        command.conflict = true;
        let previous = command.owned.clone();
        let error = orchestrate(&mut command, KeyboardImplementation::HandyKeys)
            .err()
            .unwrap();
        assert!(error.contains("Commit HandyKeys"));
        assert!(error.contains("previous configuration restored"));
        assert_eq!((command.writes, command.events), (0, 0));
        assert!(previous.iter().all(|old| command
            .owned
            .iter()
            .any(|owner| owner.same_native_owner(old))));
        assert!(
            command.current.post_process_enabled,
            "do not overwrite concurrent configuration"
        );
    }

    #[test]
    fn production_orchestration_success_merges_preferences_and_resets() {
        let mut command = RejectedCommand::new();
        command.reject = false;
        command
            .current
            .bindings
            .get_mut("transcribe")
            .unwrap()
            .current_binding = "invalid shortcut!".into();
        let result = orchestrate(&mut command, KeyboardImplementation::HandyKeys).unwrap();
        assert!(result.success);
        assert!(result.reset_bindings.contains(&"transcribe".into()));
        assert_eq!(command.selected, KeyboardImplementation::HandyKeys);
        assert_eq!((command.writes, command.events), (1, 1));
        assert_eq!(
            command.current.bindings["transcribe"].current_binding,
            settings::get_default_settings().bindings["transcribe"].current_binding
        );
        assert!(!command
            .owned
            .iter()
            .any(|entry| entry.binding.id == "cancel"
                || entry.binding.id == "transcribe_with_post_process"));
    }

    #[test]
    fn production_orchestration_uncertainty_blocks_same_backend() {
        let mut command = RejectedCommand::new();
        command.uncertain = true;
        let error = orchestrate(&mut command, KeyboardImplementation::HandyKeys)
            .err()
            .unwrap();
        assert!(error.contains("rollback incomplete or native ownership uncertain"));
        assert!(command.degraded);
        let calls = command.calls.len();
        assert!(orchestrate(&mut command, KeyboardImplementation::Tauri).is_err());
        assert_eq!(command.calls.len(), calls);
        assert_eq!((command.writes, command.events), (0, 0));
    }
}
