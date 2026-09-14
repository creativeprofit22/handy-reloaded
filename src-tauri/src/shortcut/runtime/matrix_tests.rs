//! Fault injection through the IPC's production transaction owner, with native
//! handles independent of published metadata (publication cannot repair native state).
use super::*;
use std::collections::HashMap;

struct Harness {
    current: settings::AppSettings,
    native: Vec<(Registration, usize)>,
    metadata: Vec<Registration>,
    next_handle: usize,
    target: KeyboardImplementation,
    failures: Vec<(&'static str, usize, NativeFailure)>,
    counts: HashMap<&'static str, usize>,
    publish_failure: bool,
    readiness: Option<NativeFailure>,
    writes: usize,
    initializations: usize,
    events: Vec<Vec<String>>,
    degraded: bool,
    admission: AdmissionState,
    callback_cancel: Option<bool>,
    sustained: bool,
    ready: bool,
    recording: super::super::capture::Session,
    capture_restores: usize,
    fail_capture_restore: bool,
}
impl Harness {
    fn new(target: KeyboardImplementation) -> Self {
        let mut current = settings::get_default_settings();
        current.keyboard_implementation = match target {
            KeyboardImplementation::Tauri => KeyboardImplementation::HandyKeys,
            KeyboardImplementation::HandyKeys => KeyboardImplementation::Tauri,
        };
        current.post_process_enabled = true;
        // Three deterministic, valid callback identities in both parsers.
        current.bindings.retain(|id, _| {
            matches!(
                id.as_str(),
                "transcribe" | "transcribe_with_post_process" | "cancel"
            )
        });
        for (id, text) in [
            ("transcribe", "ctrl+shift+a"),
            ("transcribe_with_post_process", "ctrl+shift+b"),
            ("cancel", "ctrl+shift+c"),
        ] {
            current.bindings.get_mut(id).unwrap().current_binding = text.into();
        }
        let mut h = Self {
            current,
            native: vec![],
            metadata: vec![],
            next_handle: 0,
            target,
            failures: vec![],
            counts: HashMap::new(),
            publish_failure: false,
            readiness: None,
            writes: 0,
            initializations: 0,
            events: vec![],
            degraded: false,
            admission: AdmissionState::default(),
            callback_cancel: None,
            sustained: false,
            ready: true,
            recording: Default::default(),
            capture_restores: 0,
            fail_capture_restore: false,
        };
        h.admission.cancel = true;
        h.seed();
        h
    }
    fn desired(&self, backend: KeyboardImplementation) -> Vec<Registration> {
        let prepared = switch::prepare(
            backend,
            &self.current.bindings,
            &settings::get_default_settings().bindings,
            self.current.post_process_enabled,
        )
        .unwrap();
        desired_native_set(
            &crate::secure_input::reconciliation::Intent {
                backend,
                bindings: &self.current.bindings,
                post_process_enabled: self.current.post_process_enabled,
                ready: self.ready,
                sustained: self.sustained,
                cancel: self.admission.cancel,
                captured: self.admission.capture,
            },
            prepared.registrations,
        )
        .unwrap()
        .0
    }
    fn seed(&mut self) {
        self.metadata = self.desired(self.current.keyboard_implementation);
        self.native = self
            .metadata
            .iter()
            .cloned()
            .enumerate()
            .map(|(handle, owner)| (owner, handle))
            .collect();
        self.next_handle = self.native.len();
    }
    fn fail(&mut self, stage: &'static str, at: usize, uncertain: bool) {
        let text = format!("{stage} fault {at}");
        self.failures.push((
            stage,
            at,
            if uncertain {
                NativeFailure::Indeterminate(text)
            } else {
                NativeFailure::Rejected(text)
            },
        ));
    }
    fn mutate(&mut self, install: bool, entry: &Registration) -> Result<(), NativeFailure> {
        // Reentry occurs inside native work, not in a separate imitation of commit.
        assert!(self.admission.enter().unwrap_err().contains("busy"));
        if let Some(cancel) = self.callback_cancel.take() {
            assert!(!self.admission.request(Some(cancel)));
        }
        let stage = match (install, entry.backend == self.target) {
            (false, false) => "RemovePrevious",
            (true, true) => "InstallCandidate",
            (false, true) => "CleanupCandidate",
            (true, false) => "RestorePrevious",
        };
        let count = self.counts.entry(stage).or_default();
        let at = *count;
        *count += 1;
        let failure = self
            .failures
            .iter()
            .find(|(s, i, _)| *s == stage && *i == at)
            .map(|(_, _, f)| f.clone());
        if matches!(failure, Some(NativeFailure::Rejected(_))) {
            return Err(failure.unwrap());
        }
        if install {
            assert!(!self
                .native
                .iter()
                .any(|(owner, _)| owner.same_native_owner(entry)));
            self.native.push((entry.clone(), self.next_handle));
            self.next_handle += 1;
        } else {
            let at = self
                .native
                .iter()
                .position(|(owner, _)| owner.same_native_owner(entry))
                .expect("removal must own a real handle");
            self.native.remove(at);
        }
        failure.map_or(Ok(()), Err)
    }
    fn set_capture(&mut self, suspended: bool) -> Result<(), String> {
        let current = self.current.clone();
        let previous = self.metadata.clone();
        let cancel = self.admission.cancel;
        let sustained = self.sustained;
        let (report, shadows) = capture_delta(self, &current, &previous, cancel, suspended, sustained)?;
        if !report.applied() { return Err(report.to_string()); }
        self.publish(&report, &shadows)?;
        self.admission.capture = suspended;
        Ok(())
    }
    fn start_recording(&mut self) {
        self.admission.enter().unwrap();
        self.set_capture(true).unwrap();
        self.recording.start(|| Ok(()), || super::super::capture::Worker::spawn(|_| {}), || panic!()).unwrap();
        self.admission.release(None);
    }
    fn run(&mut self) -> Result<ImplementationChangeResult, String> {
        self.admission.enter()?;
        let result = orchestrate(self, self.target);
        self.admission.release(None);
        result
    }
    fn assert_rejected(&self, original: &settings::AppSettings) {
        assert_eq!(
            serde_json::to_value(&self.current).unwrap(),
            serde_json::to_value(original).unwrap()
        );
        assert_eq!(self.writes, 0);
        assert!(self.events.is_empty());
    }
    /// Execute one reserved lifecycle generation using the production planner
    /// and native delta. Requests inside native callbacks reserve a later pass.
    fn reconcile_generation(&mut self) {
        assert!(self.admission.worker);
        self.admission.busy = true;
        let generation = self.admission.generation;
        let backend = self.current.keyboard_implementation;
        let previous = self.metadata.clone();
        let desired = self.desired(backend);
        let report = apply_native_delta(self, backend, &previous, &desired);
        assert!(report.applied(), "{report}");
        self.metadata = report.owned;
        self.assert_owners(&desired);
        self.admission.release(Some(generation));
    }

    fn drain_cancel(&mut self) {
        // Bounded: these tests inject at most one request during a native call.
        for _ in 0..2 {
            if !self.admission.worker {
                break;
            }
            self.reconcile_generation();
        }
        assert!(
            !self.admission.worker,
            "lifecycle work must reach quiescence"
        );
        assert!(!self.admission.busy);
        if !self.admission.cancel || cfg!(target_os = "linux") {
            assert!(
                self.native
                    .iter()
                    .all(|(entry, _)| entry.binding.id != "cancel"),
                "idle/Linux cancel leaked a native handle (including shadows): {:?}",
                self.native
            );
            assert!(self
                .metadata
                .iter()
                .all(|entry| entry.binding.id != "cancel"));
        }
    }

    fn assert_owners(&self, expected: &[Registration]) {
        assert_eq!(self.native.len(), expected.len());
        for owner in expected {
            assert!(self
                .native
                .iter()
                .any(|(actual, _)| actual.same_native_owner(owner)));
        }
    }
}
impl NativeOperations for Harness {
    fn initialize(&mut self, _: KeyboardImplementation) -> Result<(), NativeFailure> {
        self.initializations += 1;
        self.readiness.clone().map_or(Ok(()), Err)
    }
    fn install(&mut self, entry: &Registration) -> Result<(), NativeFailure> {
        self.mutate(true, entry)
    }
    fn remove(&mut self, entry: &Registration) -> Result<(), NativeFailure> {
        self.mutate(false, entry)
    }
}
impl CommandOperations for Harness {
    fn stop_capture(&mut self) -> Result<(), String> {
        let mut recording = std::mem::take(&mut self.recording);
        let result = recording.stop(|| Ok(()), || {
            self.capture_restores += 1;
            if self.fail_capture_restore {
                return Err("injected capture restore failure".into());
            }
            self.set_capture(false)
        });
        self.recording = recording;
        result
    }
    fn settings(&self) -> settings::AppSettings {
        self.current.clone()
    }
    fn snapshot(&self) -> Result<Vec<Registration>, String> {
        Ok(self.metadata.clone())
    }
    fn context(&self) -> Result<(bool, bool, bool, bool), String> {
        Ok((
            self.admission.cancel,
            self.admission.capture,
            self.ready,
            self.sustained,
        ))
    }
    fn write(&mut self, settings: settings::AppSettings) {
        self.current = settings;
        self.writes += 1;
    }
    fn publish(
        &mut self,
        report: &switch::SwitchReport,
        _: &crate::secure_input::reconciliation::Plan,
    ) -> Result<(), String> {
        if self.publish_failure {
            self.publish_failure = false;
            return Err("metadata publication rejected".into());
        }
        self.metadata = report.owned.clone();
        Ok(())
    }
    fn degraded(&mut self, error: String) {
        self.degraded = true;
        self.admission.degraded = Some(error);
    }
    fn emit(&mut self, resets: &[String]) {
        assert_eq!(self.writes, self.events.len() + 1);
        self.events.push(resets.to_vec());
    }
}
#[test]
fn capture_switch_stop_and_switch_back_restore_only_the_owned_session() {
    let mut h = Harness::new(KeyboardImplementation::Tauri);
    h.stop_capture().unwrap(); // stop before any session
    assert_eq!(h.capture_restores, 0);
    h.start_recording();
    assert!(h.admission.capture);
    assert!(h.run().unwrap().success);
    assert!(!h.admission.capture);
    assert_eq!(h.capture_restores, 1);
    h.stop_capture().unwrap(); // public stop after switching to Tauri
    h.stop_capture().unwrap();
    assert_eq!(h.capture_restores, 1);
    // A new Tauri editor owns this suspension; stale HandyKeys stop is a no-op.
    h.admission.capture = true;
    h.stop_capture().unwrap();
    assert!(h.admission.capture);
    assert!(h.run().is_err());
    h.admission.capture = false;
    h.target = KeyboardImplementation::HandyKeys;
    assert!(h.run().unwrap().success);
    h.start_recording();
    h.admission.enter().unwrap();
    h.stop_capture().unwrap();
    h.admission.release(None);
    assert_eq!(h.capture_restores, 2);
    assert!(!h.admission.capture);
}

#[test]
fn failed_capture_restore_prevents_switch_and_retains_retry_ownership() {
    let mut h = Harness::new(KeyboardImplementation::Tauri);
    h.start_recording();
    h.fail_capture_restore = true;
    let original = h.current.clone();
    assert!(h.run().err().unwrap().contains("capture-restoration-failed"));
    h.assert_rejected(&original);
    assert!(h.admission.capture);
    h.fail_capture_restore = false;
    assert!(h.run().unwrap().success);
    assert_eq!(h.capture_restores, 2);
    assert!(!h.admission.capture);
}

#[test]
fn unsupported_backend_names_do_not_reach_switch_operations() {
    for name in ["", "unknown"] {
        let mut h = Harness::new(KeyboardImplementation::Tauri);
        let original = serde_json::to_value(&h.current).unwrap();
        let result = super::super::parse_keyboard_implementation(name).and_then(|backend| {
            h.target = backend;
            h.run()
        });
        assert!(result.is_err());
        assert_eq!(h.initializations, 0);
        assert!(h.counts.is_empty(), "no native registration calls");
        assert_eq!(h.writes, 0);
        assert!(h.events.is_empty());
        assert!(!h.degraded);
        assert!(!h.admission.busy);
        assert_eq!(serde_json::to_value(&h.current).unwrap(), original);
    }
}

#[test]
fn supported_backend_names_switch_and_emit_canonical_payloads() {
    for (name, backend) in [
        ("tauri", KeyboardImplementation::Tauri),
        ("handy_keys", KeyboardImplementation::HandyKeys),
    ] {
        let mut h = Harness::new(backend);
        let parsed = super::super::parse_keyboard_implementation(name).unwrap();
        assert_eq!(parsed, backend);
        assert!(h.run().unwrap().success);
        assert_eq!(h.current.keyboard_implementation, backend);
        assert_eq!(h.events.len(), 1);
        assert_eq!(
            keyboard_implementation_event(h.current.keyboard_implementation, &h.events[0]),
            serde_json::json!({
                "setting": "keyboard_implementation",
                "value": name,
                "reset_bindings": h.events[0],
            })
        );
    }
}

fn startup_harness(backend: KeyboardImplementation) -> Harness {
    let mut h = Harness::new(backend);
    h.current.keyboard_implementation = backend;
    h.native.clear();
    h.metadata.clear();
    h.ready = false;
    h.admission.cancel = false;
    h
}

fn run_startup(h: &mut Harness) -> Result<(), String> {
    h.admission.enter()?;
    let result = super::super::startup::initialize(h);
    // Mirrors the IPC marker: only acknowledged success becomes ready.
    if result.is_ok() {
        h.ready = true;
    }
    h.admission.release(None);
    result
}

#[test]
fn startup_registration_failure_remains_retryable() {
    let mut h = startup_harness(KeyboardImplementation::Tauri);
    h.fail("InstallCandidate", 1, false);
    assert!(run_startup(&mut h).is_err());
    assert!(!h.ready);
    assert_eq!(h.writes, 0);
    assert!(h.native.is_empty());
    h.failures.clear();
    run_startup(&mut h).unwrap();
    assert!(h.ready);
    assert_eq!(
        h.native.len(),
        h.desired(KeyboardImplementation::Tauri).len()
    );
}

#[test]
fn startup_failed_fallback_does_not_persist_and_retries() {
    let mut h = startup_harness(KeyboardImplementation::HandyKeys);
    h.fail("InstallCandidate", 0, false);
    h.fail("RestorePrevious", 1, false); // Tauri installs in this harness
    assert!(run_startup(&mut h)
        .unwrap_err()
        .contains("Tauri fallback failed"));
    assert!(!h.ready);
    assert_eq!(h.writes, 0);
    assert_eq!(
        h.current.keyboard_implementation,
        KeyboardImplementation::HandyKeys
    );
    assert!(h.native.is_empty());
    h.failures.clear();
    run_startup(&mut h).unwrap();
    assert!(h.ready);
}

#[test]
fn startup_partial_cleanup_adopts_surviving_handle_on_retry() {
    let mut h = startup_harness(KeyboardImplementation::Tauri);
    h.fail("InstallCandidate", 1, false);
    h.fail("CleanupCandidate", 0, false);
    assert!(run_startup(&mut h).is_err());
    assert!(!h.ready);
    assert!(!h.degraded);
    assert_eq!(h.native.len(), 1);
    let handle = h.native[0].1;
    h.failures.clear();
    run_startup(&mut h).unwrap();
    assert!(h.ready);
    assert_eq!(h.native[0].1, handle);
    assert_eq!(
        h.native.len(),
        h.desired(KeyboardImplementation::Tauri).len()
    );
}

#[test]
fn startup_successful_fallback_is_persisted_only_after_registration() {
    let mut h = startup_harness(KeyboardImplementation::HandyKeys);
    h.fail("InstallCandidate", 0, false);
    run_startup(&mut h).unwrap();
    assert!(h.ready);
    assert_eq!(h.writes, 1);
    assert_eq!(
        h.current.keyboard_implementation,
        KeyboardImplementation::Tauri
    );
    assert!(h
        .native
        .iter()
        .all(|(r, _)| r.backend == KeyboardImplementation::Tauri));
}

const DIRECTIONS: [KeyboardImplementation; 2] = [
    KeyboardImplementation::Tauri,
    KeyboardImplementation::HandyKeys,
];

#[test]
fn every_native_and_rollback_position_both_directions() {
    for target in DIRECTIONS {
        let n = Harness::new(target).native.len();
        assert!(n >= 2);
        for stage in [
            "RemovePrevious",
            "InstallCandidate",
            "CleanupCandidate",
            "RestorePrevious",
        ] {
            for at in 0..n {
                let mut h = Harness::new(target);
                let original = h.current.clone();
                let previous = h.metadata.clone();
                // Commit rejection after complete installation exposes EVERY cleanup
                // position, including the last, unlike partial-install-only fixtures.
                h.publish_failure = matches!(stage, "CleanupCandidate" | "RestorePrevious");
                h.fail(stage, at, false);
                let error = h.run().err().unwrap();
                assert!(
                    error.contains(&format!("{stage} fault {at}")),
                    "{target:?}: {error}"
                );
                assert!(error.contains("binding '"));
                h.assert_rejected(&original);
                assert_eq!(
                    h.degraded,
                    matches!(stage, "CleanupCandidate" | "RestorePrevious")
                );
                if !h.degraded {
                    h.assert_owners(&previous);
                }
                // Published ownership must describe reality, not erase failed cleanup.
                h.assert_owners(&h.metadata);
                if stage == "CleanupCandidate" {
                    assert_eq!(h.counts["RestorePrevious"], n);
                }
            }
        }
        for cleanup in 0..n {
            for restore in 0..n {
                let mut h = Harness::new(target);
                let original = h.current.clone();
                h.publish_failure = true;
                h.fail("CleanupCandidate", cleanup, false);
                h.fail("RestorePrevious", restore, false);
                let error = h.run().err().unwrap();
                for stage in ["Commit", "CleanupCandidate", "RestorePrevious"] {
                    assert!(error.contains(stage), "{error}");
                }
                assert_eq!(h.counts["CleanupCandidate"], n);
                assert_eq!(h.counts["RestorePrevious"], n);
                h.assert_rejected(&original);
                h.assert_owners(&h.metadata);
                assert!(h.degraded);
            }
        }
    }
}

#[test]
fn lost_responses_at_every_stage_and_readiness_never_publish_success() {
    for target in DIRECTIONS {
        for uncertain in [false, true] {
            let mut h = Harness::new(target);
            let original = h.current.clone();
            h.readiness = Some(if uncertain {
                NativeFailure::Indeterminate("readiness lost".into())
            } else {
                NativeFailure::Rejected("not ready".into())
            });
            assert!(h.run().err().unwrap().contains("Initialize"));
            assert!(h.counts.is_empty());
            h.assert_rejected(&original);
            assert_eq!(h.degraded, uncertain);
        }
        let n = Harness::new(target).native.len();
        for stage in [
            "RemovePrevious",
            "InstallCandidate",
            "CleanupCandidate",
            "RestorePrevious",
        ] {
            for at in 0..n {
                let mut h = Harness::new(target);
                let original = h.current.clone();
                h.publish_failure = matches!(stage, "CleanupCandidate" | "RestorePrevious");
                h.fail(stage, at, true);
                let error = h.run().err().unwrap();
                assert!(error.contains("native ownership uncertain"), "{error}");
                h.assert_rejected(&original);
                assert!(h.degraded);
                let calls = h.counts.clone();
                h.target = original.keyboard_implementation;
                assert!(h.run().err().unwrap().contains("degraded"));
                assert_eq!(h.counts, calls);
            }
        }
    }
}

#[test]
fn rejected_reset_capture_and_disabled_postprocessing_both_directions() {
    for target in DIRECTIONS {
        for reject in [false, true] {
            let mut h = Harness::new(target);
            h.current
                .bindings
                .get_mut("transcribe")
                .unwrap()
                .current_binding = "invalid shortcut!".into();
            let original = h.current.clone();
            if reject {
                h.fail("InstallCandidate", 0, false);
            }
            let result = h.run();
            if reject {
                assert!(result.is_err());
                h.assert_rejected(&original);
            } else {
                let result = result.unwrap();
                assert_eq!(result.reset_bindings, vec!["transcribe"]);
                assert_eq!(h.events, vec![result.reset_bindings]);
                assert_eq!(
                    h.current.bindings["transcribe"].current_binding,
                    settings::get_default_settings().bindings["transcribe"].current_binding
                );
            }
        }
        let mut h = Harness::new(target);
        h.admission.capture = true;
        let original = h.current.clone();
        assert!(h.run().err().unwrap().contains("capture"));
        assert!(h.counts.is_empty());
        h.assert_rejected(&original);
        h.admission.capture = false;
        h.current.post_process_enabled = false;
        h.current
            .bindings
            .get_mut("transcribe_with_post_process")
            .unwrap()
            .current_binding = "invalid disabled binding!".into();
        h.seed();
        let result = h.run().unwrap();
        assert!(result.reset_bindings.is_empty());
        assert!(!h
            .native
            .iter()
            .any(|(owner, _)| owner.binding.id == "transcribe_with_post_process"));
    }
}

#[test]
fn cancel_callback_during_success_or_rollback_drains_final_intent() {
    for target in DIRECTIONS {
        for cancel in [false, true] {
            for reject in [false, true] {
                let mut h = Harness::new(target);
                h.admission.cancel = !cancel;
                h.seed();
                h.callback_cancel = Some(cancel);
                if reject {
                    h.fail("InstallCandidate", 0, false);
                }
                assert_eq!(h.run().is_err(), reject);
                assert!(
                    h.admission.worker,
                    "permit release must reserve pending cancel"
                );
                assert_eq!(h.admission.cancel, cancel);
                let generation = h.admission.generation;
                h.admission.busy = true;
                let desired = h.desired(h.current.keyboard_implementation);
                let previous = h.metadata.clone();
                // This is the same delta owner used by production lifecycle reconciliation.
                let backend = h.current.keyboard_implementation;
                let report = apply_native_delta(&mut h, backend, &previous, &desired);
                assert!(report.applied());
                h.assert_owners(&desired);
                assert_eq!(
                    desired
                        .iter()
                        .any(|entry| entry.roles.contains(&Role::Cancel)),
                    cancel && !cfg!(target_os = "linux")
                );
                assert!(!h.admission.release(Some(generation)));
                assert!(
                    h.admission.request(Some(!cancel)),
                    "request after handoff must schedule"
                );
            }
        }
    }
}

#[test]
fn cancel_start_switch_stop_removes_both_backend_owners() {
    for target in DIRECTIONS {
        for sustained in [false, true] {
            let mut h = Harness::new(target);
            h.sustained = sustained;
            h.admission.cancel = false;
            h.seed();
            assert!(h.admission.request(Some(true)));
            h.drain_cancel();
            h.run().unwrap();
            h.assert_owners(&h.desired(target));
            assert!(h.admission.request(Some(false)));
            h.drain_cancel();
            // The next recording must install afresh without a duplicate owner.
            assert!(h.admission.request(Some(true)));
            h.drain_cancel();
            assert!(h.admission.request(Some(false)));
            h.drain_cancel();
        }
    }
}

#[test]
fn cancel_switch_rollback_then_stop_removes_restored_owner_and_shadow() {
    for target in DIRECTIONS {
        for sustained in [false, true] {
            let mut h = Harness::new(target);
            h.sustained = sustained;
            h.seed();
            let original = h.current.clone();
            if sustained {
                // Carbon role transfer can require no installs at all. Reject
                // publication to exercise rollback even in that direction.
                h.publish_failure = true;
            } else {
                h.fail("InstallCandidate", 0, false);
            }
            assert!(h.run().is_err());
            h.assert_rejected(&original);
            assert!(!h.degraded);
            h.assert_owners(&h.desired(original.keyboard_implementation));
            assert!(h.admission.request(Some(false)));
            h.drain_cancel();
        }
    }
}

#[test]
fn delayed_cancel_install_after_stop_is_drained_not_resurrected() {
    for target in DIRECTIONS {
        for sustained in [false, true] {
            let mut h = Harness::new(target);
            h.sustained = sustained;
            h.admission.cancel = false;
            h.seed();
            assert!(h.admission.request(Some(true)));
            // Stop before the scheduled registration has even started.
            assert!(!h.admission.request(Some(false)));
            let next_handle = h.next_handle;
            h.drain_cancel();
            assert_eq!(h.next_handle, next_handle);

            assert!(h.admission.request(Some(true)));
            if !cfg!(target_os = "linux") {
                // Stop after the worker plans its install but before the native
                // acknowledgement: this intentionally completes a late install.
                h.callback_cancel = Some(false);
                h.reconcile_generation();
                assert!(h
                    .native
                    .iter()
                    .any(|(entry, _)| entry.binding.id == "cancel"));
                assert!(!h.admission.cancel);
                assert!(h.admission.worker, "late completion must schedule cleanup");
            } else {
                assert!(!h.admission.request(Some(false)));
            }
            h.drain_cancel();
        }
    }
}

#[test]
fn rapid_cancel_start_stop_switch_converges_without_replacing_held_shadows() {
    for target in DIRECTIONS {
        let mut h = Harness::new(target);
        h.sustained = true;
        h.seed();
        let held: Vec<_> = h
            .native
            .iter()
            .filter(|(entry, _)| {
                entry.backend == KeyboardImplementation::Tauri && entry.binding.id != "cancel"
            })
            .cloned()
            .collect();
        for _ in 0..8 {
            assert!(h.admission.request(Some(false)));
            assert!(!h.admission.request(Some(true)));
            assert!(!h.admission.request(Some(false)));
            assert!(h
                .run()
                .err()
                .expect("pending lifecycle must reject switch")
                .contains("busy"));
            h.drain_cancel();
            assert!(h.admission.request(Some(true)));
            h.drain_cancel();
            h.callback_cancel = Some(false);
            h.run().unwrap();
            h.drain_cancel();
            for (owner, handle) in &held {
                assert!(
                    h.native
                        .iter()
                        .any(|(entry, actual)| actual == handle && entry.same_native_owner(owner)),
                    "unchanged Carbon callback must retain its outstanding release"
                );
            }
            h.target = match h.target {
                KeyboardImplementation::Tauri => KeyboardImplementation::HandyKeys,
                KeyboardImplementation::HandyKeys => KeyboardImplementation::Tauri,
            };
        }
    }
}

#[test]
fn held_native_handle_survives_production_shadow_role_transfer_both_directions() {
    let mut h = Harness::new(KeyboardImplementation::HandyKeys);
    h.sustained = true;
    h.seed();
    let held = h
        .native
        .iter()
        .find(|(entry, _)| entry.binding.id == "transcribe")
        .unwrap()
        .clone();
    h.run().unwrap();
    let shadow = h
        .metadata
        .iter()
        .find(|entry| entry.same_native_owner(&held.0))
        .unwrap();
    assert_eq!(shadow.roles, vec![Role::Shadow]);
    assert!(h
        .native
        .iter()
        .any(|(entry, handle)| *handle == held.1 && entry.same_native_owner(&held.0)));
    h.target = KeyboardImplementation::Tauri;
    h.run().unwrap();
    assert_eq!(
        h.metadata
            .iter()
            .find(|entry| entry.same_native_owner(&held.0))
            .unwrap()
            .roles,
        vec![Role::Primary]
    );
    assert!(
        h.native
            .iter()
            .any(|(entry, handle)| *handle == held.1 && entry.same_native_owner(&held.0)),
        "outstanding release still resolves to its original callback handle"
    );
}
