use super::*;
use crate::shortcut::{
    runtime::desired_native_set,
    switch::{self, NativeFailure, NativeOperations},
};

fn binding(id: &str, text: &str) -> ShortcutBinding {
    ShortcutBinding {
        id: id.into(),
        name: id.into(),
        description: String::new(),
        current_binding: text.into(),
        default_binding: text.into(),
    }
}
fn intent(bindings: &HashMap<String, ShortcutBinding>) -> Intent<'_> {
    Intent {
        backend: KeyboardImplementation::HandyKeys,
        bindings,
        post_process_enabled: true,
        ready: true,
        sustained: true,
        cancel: true,
        captured: false,
    }
}
fn bindings() -> HashMap<String, ShortcutBinding> {
    [
        binding("transcribe", "alt+space"),
        binding("cancel", "escape"),
        binding("transcribe_with_post_process", "ctrl+shift+p"),
    ]
    .into_iter()
    .map(|b| (b.id.clone(), b))
    .collect()
}

#[test]
fn explicit_intent_respects_backend_readiness_sustained_capture_cancel_and_post_processing() {
    let bindings = bindings();
    let mut intent = intent(&bindings);
    assert!(plan(&intent)
        .registrations
        .iter()
        .any(|r| r.binding.id == "transcribe"));
    intent.backend = KeyboardImplementation::Tauri;
    assert!(plan(&intent).registrations.is_empty());
    intent.backend = KeyboardImplementation::HandyKeys;
    intent.ready = false;
    assert!(plan(&intent).registrations.is_empty());
    intent.ready = true;
    intent.sustained = false;
    assert!(plan(&intent).registrations.is_empty());
    intent.sustained = true;
    intent.post_process_enabled = false;
    intent.cancel = false;
    let planned = plan(&intent);
    assert_eq!(planned.registrations.len(), 1);
    assert_eq!(planned.registrations[0].binding.id, "transcribe");
    intent.captured = true;
    assert!(plan(&intent).registrations.is_empty());
    intent.cancel = true;
    assert_eq!(
        plan(&intent).registrations.len(),
        usize::from(!cfg!(target_os = "linux"))
    );
}

#[test]
fn conversion_preserves_immunity_widening_and_uncovered_rules() {
    let bindings: HashMap<_, _> = [
        binding("immune", "ctrl+shift"),
        binding("wide", "alt_left+space"),
        binding("fn", "fn+space"),
        binding("invalid", "not-a-shortcut"),
    ]
    .into_iter()
    .map(|b| (b.id.clone(), b))
    .collect();
    let planned = plan(&intent(&bindings));
    assert!(!planned
        .registrations
        .iter()
        .any(|r| r.binding.id == "immune"));
    assert_eq!(planned.degraded, vec!["wide"]);
    assert_eq!(planned.uncovered, vec!["fn", "invalid"]);
}

#[derive(Default)]
struct Native {
    owners: Vec<(Registration, usize)>,
    next_handle: usize,
    calls: usize,
    fail_remove: Option<String>,
    fail_install: Option<String>,
    uncertain: bool,
}
impl Native {
    fn seeded(owners: &[Registration]) -> Self {
        Self {
            owners: owners
                .iter()
                .cloned()
                .enumerate()
                .map(|(i, r)| (r, i))
                .collect(),
            next_handle: owners.len(),
            ..Self::default()
        }
    }
}
impl NativeOperations for Native {
    fn initialize(&mut self, _: KeyboardImplementation) -> Result<(), NativeFailure> {
        Ok(())
    }
    fn remove(&mut self, r: &Registration) -> Result<(), NativeFailure> {
        self.calls += 1;
        if self.fail_remove.as_deref() == Some(&r.binding.id) {
            return Err(if self.uncertain {
                NativeFailure::Indeterminate("lost removal response".into())
            } else {
                NativeFailure::Rejected("remove rejected".into())
            });
        }
        self.owners.retain(|(owner, _)| !owner.same_native_owner(r));
        Ok(())
    }
    fn install(&mut self, r: &Registration) -> Result<(), NativeFailure> {
        self.calls += 1;
        if self.fail_install.as_deref() == Some(&r.binding.id) {
            return Err("install rejected".into());
        }
        self.owners.push((r.clone(), self.next_handle));
        self.next_handle += 1;
        Ok(())
    }
}

#[test]
fn every_failed_shadow_removal_retains_ownership_and_can_be_retried() {
    let bindings = bindings();
    let planned = plan(&intent(&bindings));
    for failed in &planned.registrations {
        let mut native = Native::seeded(&planned.registrations);
        native.fail_remove = Some(failed.binding.id.clone());
        let report = switch::apply(
            &mut native,
            KeyboardImplementation::HandyKeys,
            &planned.registrations,
            &[],
        );
        assert!(!report.applied());
        assert!(report.rollback_complete());
        assert!(report.owned.iter().any(|r| r.same_native_owner(failed)));
        let status = coverage(&Plan::default(), &report.owned, &report.uncertain);
        assert!(status.registered.iter().any(|b| b.id == failed.binding.id));
        assert!(status.uncovered.contains(&failed.binding.id));
        native.fail_remove = None;
        let retry = switch::apply(
            &mut native,
            KeyboardImplementation::HandyKeys,
            &report.owned,
            &[],
        );
        assert!(retry.applied());
        assert!(native.owners.is_empty());
    }
}

#[test]
fn every_required_shadow_install_failure_is_part_of_transaction_report() {
    let bindings = bindings();
    let (desired, planned) = desired_native_set(&intent(&bindings), vec![]).unwrap();
    for failed in &planned.registrations {
        let mut native = Native {
            fail_install: Some(failed.binding.id.clone()),
            ..Native::default()
        };
        let report = switch::apply(
            &mut native,
            KeyboardImplementation::HandyKeys,
            &[],
            &desired,
        );
        assert!(!report.applied());
        assert!(report
            .installation
            .errors
            .iter()
            .any(|e| e.backend == KeyboardImplementation::Tauri
                && e.registration.as_ref().unwrap().binding.id == failed.binding.id));
        assert!(report.rollback_complete());
        assert!(report.owned.is_empty());
        assert!(coverage(&planned, &report.owned, &report.uncertain)
            .uncovered
            .contains(&failed.binding.id));
    }
}

#[test]
fn unchanged_shadow_primary_transfers_preserve_original_callback_and_held_release_handle() {
    let bindings: HashMap<_, _> = [binding("transcribe", "alt+space")]
        .into_iter()
        .map(|b| (b.id.clone(), b))
        .collect();
    let mut explicit = intent(&bindings);
    let shadow = plan(&explicit).registrations.remove(0);
    let mut primary = shadow.clone();
    primary.roles = vec![Role::Primary];
    let mut native = Native::seeded(&[shadow.clone()]);
    // Simulated press is associated with Carbon handle 0, not its role.
    let held_handle = native.owners[0].1;
    explicit.backend = KeyboardImplementation::Tauri;
    let (desired, _) = desired_native_set(&explicit, vec![primary]).unwrap();
    let to_primary = switch::apply(&mut native, explicit.backend, &[shadow], &desired);
    assert!(to_primary.applied());
    assert_eq!(to_primary.owned[0].roles, vec![Role::Primary]);
    explicit.backend = KeyboardImplementation::HandyKeys;
    let (desired, _) = desired_native_set(&explicit, vec![]).unwrap();
    let to_shadow = switch::apply(&mut native, explicit.backend, &to_primary.owned, &desired);
    assert!(to_shadow.applied());
    assert!(to_shadow
        .owned
        .iter()
        .any(|r| r.roles == vec![Role::Shadow]));
    assert_eq!(native.calls, 0);
    assert_eq!(
        native.owners[0].1, held_handle,
        "original registration can still deliver release"
    );
}

#[test]
fn uncertain_shadow_removal_is_not_reported_as_covered() {
    let bindings = bindings();
    let planned = plan(&intent(&bindings));
    let failed = &planned.registrations[0];
    let mut native = Native::seeded(&planned.registrations);
    native.fail_remove = Some(failed.binding.id.clone());
    native.uncertain = true;
    let report = switch::apply(
        &mut native,
        KeyboardImplementation::HandyKeys,
        &planned.registrations,
        &[],
    );
    assert!(!report.rollback_complete());
    let status = coverage(&planned, &report.owned, &report.uncertain);
    assert!(!status.covered.contains(&failed.binding.id));
    assert!(status.uncovered.contains(&failed.binding.id));
    assert!(status.registered.iter().any(|b| b.id == failed.binding.id));
}

#[test]
fn widened_shadow_collision_rejects_before_native_work() {
    let bindings: HashMap<_, _> = [
        binding("left", "alt_left+space"),
        binding("right", "alt_right+space"),
    ]
    .into_iter()
    .map(|b| (b.id.clone(), b))
    .collect();
    assert!(desired_native_set(&intent(&bindings), vec![])
        .unwrap_err()
        .contains("conflict"));
}
