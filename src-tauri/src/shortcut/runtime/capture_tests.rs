//! Injected native failures through the production capture planner and batch owner.
use super::*;

#[derive(Default)]
struct NativeHarness {
    owned: Vec<Registration>,
    calls: Vec<(bool, String)>,
    fail: Vec<usize>,
    uncertain: bool,
    initialize_failure: bool,
}
impl NativeOperations for NativeHarness {
    fn initialize(&mut self, _: KeyboardImplementation) -> Result<(), NativeFailure> {
        if self.initialize_failure {
            Err("initialization rejected".into())
        } else {
            Ok(())
        }
    }
    fn remove(&mut self, entry: &Registration) -> Result<(), NativeFailure> {
        self.operation(false, entry)?;
        self.owned.retain(|owner| !owner.same_native_owner(entry));
        Ok(())
    }
    fn install(&mut self, entry: &Registration) -> Result<(), NativeFailure> {
        self.operation(true, entry)?;
        assert!(!self
            .owned
            .iter()
            .any(|owner| owner.same_native_owner(entry)));
        self.owned.push(entry.clone());
        Ok(())
    }
}
impl NativeHarness {
    fn operation(&mut self, install: bool, entry: &Registration) -> Result<(), NativeFailure> {
        self.calls.push((install, entry.binding.id.clone()));
        if self.fail.contains(&self.calls.len()) {
            return Err(if self.uncertain {
                NativeFailure::Indeterminate("injected lost response".into())
            } else {
                NativeFailure::Rejected("injected native rejection".into())
            });
        }
        Ok(())
    }
}
fn settings(backend: KeyboardImplementation, post: bool) -> settings::AppSettings {
    let mut current = settings::get_default_settings();
    current.keyboard_implementation = backend;
    current.post_process_enabled = post;
    for (id, binding) in &mut current.bindings {
        binding.current_binding = match id.as_str() {
            "transcribe" => "ctrl+shift+a",
            "transcribe_with_post_process" => "ctrl+shift+b",
            "cancel" => "escape",
            _ => panic!("assign a unique shortcut for {id}"),
        }
        .into();
    }
    current
}
fn restored(current: &settings::AppSettings, cancel: bool, sustained: bool) -> Vec<Registration> {
    let mut native = NativeHarness::default();
    let (report, _) = capture_delta(&mut native, current, &[], cancel, false, sustained).unwrap();
    assert!(report.applied(), "{report}");
    report.owned
}
fn same_owners(a: &[Registration], b: &[Registration]) -> bool {
    a.len() == b.len()
        && a.iter()
            .all(|entry| b.iter().any(|owner| owner.same_native_owner(entry)))
}

#[test]
fn every_capture_removal_and_restoration_failure_is_reported_and_compensated() {
    for backend in [
        KeyboardImplementation::Tauri,
        KeyboardImplementation::HandyKeys,
    ] {
        let current = settings(backend, true);
        // Sustained mode exercises real primary + Carbon-shadow planning even on Windows.
        let previous = restored(&current, false, true);
        for suspended in [true, false] {
            for failed in 1..=previous.len() {
                let before = if suspended { previous.clone() } else { vec![] };
                let mut native = NativeHarness {
                    owned: before.clone(),
                    fail: vec![failed],
                    ..Default::default()
                };
                let (report, _) =
                    capture_delta(&mut native, &current, &before, false, suspended, true).unwrap();
                assert!(!report.applied());
                assert!(report.rollback_complete(), "{report}");
                let batch = if suspended {
                    &report.removal
                } else {
                    &report.installation
                };
                assert_eq!(batch.errors.len(), 1);
                assert_eq!(batch.changed.len(), previous.len() - 1);
                assert!(report
                    .to_string()
                    .contains(&batch.errors[0].registration.as_ref().unwrap().binding.id));
                assert!(same_owners(&before, &report.owned));
                assert!(same_owners(&before, &native.owned));
            }
        }
    }
}

#[test]
fn capture_compensation_failures_and_uncertainty_never_claim_success() {
    for backend in [
        KeyboardImplementation::Tauri,
        KeyboardImplementation::HandyKeys,
    ] {
        let current = settings(backend, true);
        let previous = restored(&current, false, true);
        for suspended in [true, false] {
            for compensation in 1..previous.len() {
                let before = if suspended { previous.clone() } else { vec![] };
                let mut native = NativeHarness {
                    owned: before.clone(),
                    fail: vec![1, previous.len() + compensation],
                    ..Default::default()
                };
                let (report, _) =
                    capture_delta(&mut native, &current, &before, false, suspended, true).unwrap();
                assert!(!report.applied());
                assert!(!report.rollback_complete());
                let compensation = if suspended {
                    &report.restoration
                } else {
                    &report.cleanup
                };
                assert_eq!(compensation.errors.len(), 1);
                assert!(report.to_string().contains("rollback incomplete"));
                assert!(same_owners(&native.owned, &report.owned));
            }
            let before = if suspended { previous.clone() } else { vec![] };
            let mut native = NativeHarness {
                owned: before.clone(),
                fail: vec![1],
                uncertain: true,
                ..Default::default()
            };
            let (report, _) =
                capture_delta(&mut native, &current, &before, false, suspended, true).unwrap();
            assert!(!report.applied());
            assert!(!report.rollback_complete());
            assert!(!report.uncertain.is_empty());
        }
    }
}

#[test]
fn capture_initialization_failure_does_not_mutate_ownership() {
    let current = settings(KeyboardImplementation::HandyKeys, true);
    for suspended in [true, false] {
        let previous = if suspended {
            restored(&current, false, true)
        } else {
            vec![]
        };
        let mut native = NativeHarness {
            owned: previous.clone(),
            initialize_failure: true,
            ..Default::default()
        };
        let (report, _) =
            capture_delta(&mut native, &current, &previous, false, suspended, true).unwrap();
        assert!(!report.applied());
        assert!(report.rollback_complete());
        assert!(native.calls.is_empty());
        assert!(same_owners(&previous, &report.owned));
    }
}

#[test]
fn repeated_resume_retains_correct_handles_and_capture_excludes_disabled_postprocessing() {
    for backend in [
        KeyboardImplementation::Tauri,
        KeyboardImplementation::HandyKeys,
    ] {
        for post in [true, false] {
            for cancel in [true, false] {
                let current = settings(backend, post);
                let previous = restored(&current, cancel, true);
                assert_eq!(
                    previous
                        .iter()
                        .any(|entry| entry.binding.id == "transcribe_with_post_process"),
                    post
                );
                let mut native = NativeHarness {
                    owned: previous.clone(),
                    ..Default::default()
                };
                let (report, _) =
                    capture_delta(&mut native, &current, &previous, cancel, false, true).unwrap();
                assert!(report.applied());
                assert!(
                    native.calls.is_empty(),
                    "repeated resume must never replace correct handles"
                );
                let (suspended, _) =
                    capture_delta(&mut native, &current, &previous, cancel, true, true).unwrap();
                assert!(suspended.applied());
                assert!(suspended
                    .owned
                    .iter()
                    .all(|entry| entry.binding.id == "cancel"));
                assert!(native.calls.iter().all(|(_, id)| id != "cancel"));
                // A queued Secure Input reconciliation observes capture intent and cannot undo it.
                let explicit = crate::secure_input::reconciliation::Intent {
                    backend,
                    bindings: &current.bindings,
                    post_process_enabled: post,
                    ready: true,
                    sustained: true,
                    cancel,
                    captured: true,
                };
                let (desired, _) = desired_native_set(&explicit, previous.clone()).unwrap();
                assert!(same_owners(&desired, &suspended.owned));
                let (resumed, _) =
                    capture_delta(&mut native, &current, &suspended.owned, cancel, false, true)
                        .unwrap();
                assert!(resumed.applied());
                assert!(same_owners(&previous, &resumed.owned));
                assert_eq!(
                    resumed
                        .owned
                        .iter()
                        .any(|entry| entry.binding.id == "cancel"),
                    cancel && !cfg!(target_os = "linux")
                );
            }
        }
    }
}
