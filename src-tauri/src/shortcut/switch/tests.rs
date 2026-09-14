use super::*;

fn binding(id: &str, text: &str) -> ShortcutBinding {
    ShortcutBinding {
        id: id.into(),
        name: id.into(),
        description: String::new(),
        default_binding: text.into(),
        current_binding: text.into(),
    }
}

fn registrations(backend: KeyboardImplementation) -> Vec<Registration> {
    [("a", "ctrl+a"), ("b", "ctrl+b"), ("c", "ctrl+c")]
        .into_iter()
        .map(|(id, text)| Registration::new(backend, binding(id, text), Role::Primary).unwrap())
        .collect()
}

#[derive(Default)]
struct FakeNative {
    owned: Vec<Registration>,
    calls: Vec<(bool, KeyboardImplementation, String)>,
    failures: Vec<(usize, NativeFailure)>,
    initialize_error: Option<NativeFailure>,
}

impl FakeNative {
    fn mutate(&mut self, install: bool, registration: &Registration) -> Result<(), NativeFailure> {
        let index = self.calls.len();
        self.calls.push((
            install,
            registration.backend,
            registration.binding.id.clone(),
        ));
        if let Some((_, failure)) = self.failures.iter().find(|(at, _)| *at == index) {
            // An indeterminate response deliberately hides a successful native
            // operation, proving that the report cannot assume nothing happened.
            if matches!(failure, NativeFailure::Rejected(_)) {
                return Err(failure.clone());
            }
        }
        if install {
            assert!(!self
                .owned
                .iter()
                .any(|entry| entry.same_native_owner(registration)));
            self.owned.push(registration.clone());
        } else {
            assert!(self
                .owned
                .iter()
                .any(|entry| entry.same_native_owner(registration)));
            forget(&mut self.owned, registration);
        }
        match self.failures.iter().find(|(at, _)| *at == index) {
            Some((_, failure)) => Err(failure.clone()),
            None => Ok(()),
        }
    }
}

impl NativeOperations for FakeNative {
    fn initialize(&mut self, _: KeyboardImplementation) -> Result<(), NativeFailure> {
        self.initialize_error.clone().map_or(Ok(()), Err)
    }
    fn remove(&mut self, registration: &Registration) -> Result<(), NativeFailure> {
        self.mutate(false, registration)
    }
    fn install(&mut self, registration: &Registration) -> Result<(), NativeFailure> {
        self.mutate(true, registration)
    }
}

fn fixture() -> (FakeNative, Vec<Registration>, Vec<Registration>) {
    let previous = registrations(KeyboardImplementation::Tauri);
    let desired = registrations(KeyboardImplementation::HandyKeys);
    let native = FakeNative {
        owned: previous.clone(),
        ..FakeNative::default()
    };
    (native, previous, desired)
}

fn reject(index: usize) -> (usize, NativeFailure) {
    (
        index,
        NativeFailure::Rejected(format!("injected failure {index}")),
    )
}

fn owners_equal(left: &[Registration], right: &[Registration]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .all(|entry| right.iter().any(|other| entry.same_native_owner(other)))
}

#[test]
fn compatible_roles_fold_but_conflicting_callbacks_reject() {
    let primary = registrations(KeyboardImplementation::Tauri).remove(0);
    let mut shadow = primary.clone();
    shadow.roles = vec![Role::Shadow];
    let folded = fold_roles(vec![primary.clone(), shadow.clone()]).unwrap();
    assert_eq!(folded.len(), 1);
    assert_eq!(folded[0].roles, vec![Role::Primary, Role::Shadow]);
    shadow.binding.id = "conflicting".into();
    assert!(fold_roles(vec![primary, shadow]).is_err());
}

#[test]
fn errors_name_every_stage_backend_and_binding() {
    let (mut native, previous, desired) = fixture();
    native.failures = vec![reject(4), reject(6), reject(8)];
    let report = apply(
        &mut native,
        KeyboardImplementation::HandyKeys,
        &previous,
        &desired,
    );
    let message = report.to_string();
    for expected in [
        "rollback incomplete",
        "InstallCandidate HandyKeys",
        "CleanupCandidate HandyKeys",
        "RestorePrevious Tauri",
        "binding '",
        "injected failure 4",
        "injected failure 6",
        "injected failure 8",
    ] {
        assert!(message.contains(expected), "missing {expected}: {message}");
    }
}

#[test]
fn uncertain_readiness_is_not_reported_as_restored() {
    let (mut native, previous, desired) = fixture();
    native.initialize_error = Some(NativeFailure::Indeterminate("readiness lost".into()));
    let report = apply(
        &mut native,
        KeyboardImplementation::HandyKeys,
        &previous,
        &desired,
    );
    assert!(!report.rollback_complete());
    assert!(report.to_string().contains("Initialize HandyKeys"));
    assert!(native.calls.is_empty());
}

#[test]
fn initialization_failure_does_not_remove_previous_or_permit_commit() {
    let (mut native, previous, desired) = fixture();
    native.initialize_error = Some(NativeFailure::Rejected("not ready".into()));
    let report = apply(
        &mut native,
        KeyboardImplementation::HandyKeys,
        &previous,
        &desired,
    );
    assert!(!report.applied());
    assert!(report.rollback_complete());
    assert!(native.calls.is_empty());
    assert!(owners_equal(&native.owned, &previous));
}

#[test]
fn every_removal_failure_restores_only_acknowledged_removals() {
    for position in 0..3 {
        let (mut native, previous, desired) = fixture();
        native.failures.push(reject(position));
        let report = apply(
            &mut native,
            KeyboardImplementation::HandyKeys,
            &previous,
            &desired,
        );
        assert!(!report.applied());
        assert!(report.rollback_complete());
        assert!(report.installation.changed.is_empty());
        assert_eq!(report.removal.changed.len(), 2);
        assert_eq!(report.removal.retained.len(), 1);
        assert_eq!(report.restoration.changed.len(), 2);
        assert!(native
            .calls
            .iter()
            .all(|(_, backend, _)| *backend == KeyboardImplementation::Tauri));
        assert!(owners_equal(&native.owned, &previous));
        assert!(owners_equal(&report.owned, &native.owned));
    }
}

#[test]
fn every_install_failure_cleans_candidates_and_restores_previous() {
    for position in 3..6 {
        let (mut native, previous, desired) = fixture();
        native.failures.push(reject(position));
        let report = apply(
            &mut native,
            KeyboardImplementation::HandyKeys,
            &previous,
            &desired,
        );
        assert!(!report.applied());
        assert!(report.rollback_complete());
        assert_eq!(report.installation.changed.len(), 2);
        assert_eq!(report.cleanup.changed.len(), 2);
        assert_eq!(report.restoration.changed.len(), 3);
        assert!(owners_equal(&native.owned, &previous));
        assert!(owners_equal(&report.owned, &native.owned));
        let cleanup_ids: Vec<_> = native.calls[6..8]
            .iter()
            .map(|(_, _, id)| id.as_str())
            .collect();
        let installed_ids: Vec<_> = report
            .installation
            .changed
            .iter()
            .rev()
            .map(|entry| entry.binding.id.as_str())
            .collect();
        assert_eq!(cleanup_ids, installed_ids);
    }
}

#[test]
fn cleanup_and_restoration_failures_are_independent_and_never_forgotten() {
    for rollback_failures in [vec![6], vec![8], vec![6, 8]] {
        let (mut native, previous, desired) = fixture();
        native.failures.push(reject(4));
        native
            .failures
            .extend(rollback_failures.iter().copied().map(reject));
        let report = apply(
            &mut native,
            KeyboardImplementation::HandyKeys,
            &previous,
            &desired,
        );
        assert!(!report.applied());
        assert!(!report.rollback_complete());
        assert_eq!(
            native.calls.len(),
            11,
            "rollback must continue after errors"
        );
        assert_eq!(report.installation.errors.len(), 1);
        assert_eq!(
            report.cleanup.errors.len(),
            usize::from(rollback_failures.contains(&6))
        );
        assert_eq!(
            report.restoration.errors.len(),
            usize::from(rollback_failures.contains(&8))
        );
        assert!(owners_equal(&report.owned, &native.owned));
    }
}

#[test]
fn lost_install_response_cannot_claim_clean_rollback() {
    let (mut native, previous, desired) = fixture();
    native
        .failures
        .push((4, NativeFailure::Indeterminate("response lost".into())));
    let report = apply(
        &mut native,
        KeyboardImplementation::HandyKeys,
        &previous,
        &desired,
    );
    assert!(!report.applied());
    assert!(!report.rollback_complete());
    assert_eq!(report.uncertain.len(), 1);
    assert_eq!(report.uncertain[0].binding.id, "b");
    assert!(native
        .owned
        .iter()
        .any(|entry| entry.same_native_owner(&desired[1])));
    assert_eq!(report.cleanup.changed.len(), 2);
}

#[test]
fn lost_removal_response_is_not_blindly_restored() {
    let (mut native, previous, desired) = fixture();
    native
        .failures
        .push((1, NativeFailure::Indeterminate("response lost".into())));
    let report = apply(
        &mut native,
        KeyboardImplementation::HandyKeys,
        &previous,
        &desired,
    );
    assert!(!report.rollback_complete());
    assert_eq!(report.uncertain.len(), 1);
    assert_eq!(report.restoration.changed.len(), 2);
    assert!(report.installation.changed.is_empty());
    assert!(!native.calls[3..].iter().any(|(_, _, id)| id == "b"));
}

#[test]
fn success_permits_one_external_commit_only_after_all_native_calls() {
    let (mut native, previous, desired) = fixture();
    let report = apply(
        &mut native,
        KeyboardImplementation::HandyKeys,
        &previous,
        &desired,
    );
    let mut commits = 0;
    let mut events = 0;
    if report.applied() {
        assert_eq!(native.calls.len(), 6);
        commits += 1;
        events += 1;
    }
    assert_eq!((commits, events), (1, 1));
    assert!(owners_equal(&report.owned, &desired));
    assert!(report.cleanup.changed.is_empty());
}

#[test]
fn rejected_application_never_permits_settings_or_event_publication() {
    for position in 0..6 {
        let (mut native, previous, desired) = fixture();
        native.failures.push(reject(position));
        let report = apply(
            &mut native,
            KeyboardImplementation::HandyKeys,
            &previous,
            &desired,
        );
        assert!(
            !report.applied(),
            "failure at {position} incorrectly allowed commit"
        );
    }
}

#[test]
fn role_transfer_keeps_native_callback_and_rolls_back_roles_on_failure() {
    let (mut native, mut previous, _) = fixture();
    previous[0].roles = vec![Role::Shadow];
    native.owned = previous.clone();
    let mut desired = previous.clone();
    desired[0].roles = vec![Role::Primary, Role::Cancel];
    let report = apply(
        &mut native,
        KeyboardImplementation::Tauri,
        &previous,
        &desired,
    );
    assert!(report.applied());
    assert!(native.calls.is_empty());
    assert_eq!(report.owned[0].roles, vec![Role::Primary, Role::Cancel]);

    desired[1] = Registration::new(
        KeyboardImplementation::Tauri,
        binding("b", "ctrl+d"),
        Role::Primary,
    )
    .unwrap();
    native.failures.push(reject(1));
    let report = apply(
        &mut native,
        KeyboardImplementation::Tauri,
        &previous,
        &desired,
    );
    assert!(report.rollback_complete());
    assert_eq!(
        report
            .owned
            .iter()
            .find(|entry| entry.binding.id == "a")
            .unwrap()
            .roles,
        vec![Role::Shadow]
    );
}

#[test]
fn preparation_uses_native_parser_and_validates_defaults() {
    let defaults = HashMap::from([("a".into(), binding("a", "ctrl+a"))]);
    let invalid = HashMap::from([("a".into(), binding("a", "ctrl+not-a-key"))]);
    let plan = prepare(KeyboardImplementation::Tauri, &invalid, &defaults, true).unwrap();
    assert_eq!(plan.backend, KeyboardImplementation::Tauri);
    assert_eq!(plan.resets.len(), 1);
    assert_eq!(plan.registrations[0].binding.current_binding, "ctrl+a");
    assert!(prepare(KeyboardImplementation::Tauri, &invalid, &invalid, true).is_err());
    assert_eq!(
        invalid["a"].current_binding, "ctrl+not-a-key",
        "preparation never changes settings"
    );
}

#[test]
fn preparation_rejects_native_alias_collisions() {
    for backend in [
        KeyboardImplementation::Tauri,
        KeyboardImplementation::HandyKeys,
    ] {
        let bindings = HashMap::from([
            ("a".into(), binding("a", "ctrl+a")),
            ("b".into(), binding("b", "control+a")),
        ]);
        assert!(prepare(backend, &bindings, &bindings, true).is_err());
    }
}

#[test]
fn preparation_is_sorted_and_excludes_dynamic_and_disabled_bindings() {
    let bindings = HashMap::from([
        ("z".into(), binding("z", "ctrl+z")),
        ("a".into(), binding("a", "ctrl+a")),
        ("cancel".into(), binding("cancel", "invalid")),
        (
            "transcribe_with_post_process".into(),
            binding("transcribe_with_post_process", "invalid"),
        ),
    ]);
    let plan = prepare(KeyboardImplementation::Tauri, &bindings, &bindings, false).unwrap();
    let ids: Vec<_> = plan
        .registrations
        .iter()
        .map(|entry| entry.binding.id.as_str())
        .collect();
    assert_eq!(ids, ["a", "z"]);
    assert!(plan.resets.is_empty());
}

#[test]
fn reports_identify_original_and_rollback_failures() {
    let (mut native, previous, desired) = fixture();
    native.failures = vec![reject(4), reject(6), reject(8)];
    let report = apply(
        &mut native,
        KeyboardImplementation::HandyKeys,
        &previous,
        &desired,
    );
    let error = &report.installation.errors[0];
    assert_eq!(error.stage, Stage::InstallCandidate);
    assert_eq!(error.backend, KeyboardImplementation::HandyKeys);
    assert_eq!(error.registration.as_ref().unwrap().binding.id, "b");
    assert_eq!(
        error.failure,
        NativeFailure::Rejected("injected failure 4".into())
    );
    assert_eq!(report.cleanup.errors[0].stage, Stage::CleanupCandidate);
    assert_eq!(report.restoration.errors[0].stage, Stage::RestorePrevious);
}
