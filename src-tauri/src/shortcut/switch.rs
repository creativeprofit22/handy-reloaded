//! Backend-independent switch preparation and native transaction reports.
//!
//! The caller owns operation admission and the settings snapshot. Native batches
//! never load or publish settings. Only a successful application permits commit.

use std::collections::HashMap;

use crate::settings::{KeyboardImplementation, ShortcutBinding};
use handy_keys::Hotkey;
use tauri_plugin_global_shortcut::Shortcut;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum NativeIdentity {
    Tauri(Shortcut),
    HandyKeys(Hotkey),
}

pub(crate) fn parse_identity(
    backend: KeyboardImplementation,
    text: &str,
) -> Result<NativeIdentity, String> {
    match backend {
        KeyboardImplementation::Tauri => {
            super::tauri_impl::validate_shortcut(text)?;
            text.parse::<Shortcut>()
                .map(NativeIdentity::Tauri)
                .map_err(|error| format!("Invalid Tauri shortcut '{text}': {error}"))
        }
        KeyboardImplementation::HandyKeys => text
            .parse::<Hotkey>()
            .map(NativeIdentity::HandyKeys)
            .map_err(|error| format!("Invalid HandyKeys shortcut '{text}': {error}")),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Role {
    Primary,
    Shadow,
    Cancel,
}

#[derive(Clone, Debug)]
pub(crate) struct Registration {
    pub backend: KeyboardImplementation,
    pub binding: ShortcutBinding,
    pub identity: NativeIdentity,
    pub roles: Vec<Role>,
}

impl Registration {
    pub(crate) fn new(
        backend: KeyboardImplementation,
        binding: ShortcutBinding,
        role: Role,
    ) -> Result<Self, String> {
        let identity = parse_identity(backend, &binding.current_binding)?;
        Ok(Self {
            backend,
            binding,
            identity,
            roles: vec![role],
        })
    }

    /// Roles are not native identity: adopting a Carbon shadow must keep its
    /// original callback and handle, including any outstanding release delivery.
    pub(crate) fn same_native_owner(&self, other: &Self) -> bool {
        self.backend == other.backend
            && self.identity == other.identity
            && self.binding.id == other.binding.id
    }
}

#[derive(Debug)]
pub(crate) struct CandidatePlan {
    pub backend: KeyboardImplementation,
    pub registrations: Vec<Registration>,
    pub resets: Vec<ShortcutBinding>,
}

/// Deterministic preparation with real native parsers, including replacements.
/// Dynamic cancel and disabled post-processing are never static candidates.
pub(crate) fn prepare(
    backend: KeyboardImplementation,
    bindings: &HashMap<String, ShortcutBinding>,
    defaults: &HashMap<String, ShortcutBinding>,
    post_process_enabled: bool,
) -> Result<CandidatePlan, String> {
    let mut ids: Vec<_> = defaults.keys().chain(bindings.keys()).collect();
    ids.sort();
    ids.dedup();
    let mut plan = CandidatePlan {
        backend,
        registrations: Vec::new(),
        resets: Vec::new(),
    };
    for id in ids {
        if id == "cancel" || (id == "transcribe_with_post_process" && !post_process_enabled) {
            continue;
        }
        let mut binding = bindings
            .get(id)
            .or_else(|| defaults.get(id))
            .cloned()
            .ok_or_else(|| format!("Missing binding '{id}'"))?;
        if &binding.id != id {
            return Err(format!("Binding ID does not match settings key '{id}'"));
        }
        if parse_identity(backend, &binding.current_binding).is_err() {
            let default = defaults
                .get(id)
                .ok_or_else(|| format!("No compatible default for binding '{id}'"))?;
            parse_identity(backend, &default.current_binding)
                .map_err(|error| format!("Invalid default for '{id}': {error}"))?;
            binding.current_binding = default.current_binding.clone();
            plan.resets.push(binding.clone());
        }
        let registration = Registration::new(backend, binding, Role::Primary)?;
        if let Some(previous) = plan
            .registrations
            .iter()
            .find(|previous| previous.identity == registration.identity)
        {
            return Err(format!(
                "Bindings '{}' and '{}' use the same native shortcut",
                previous.binding.id, registration.binding.id
            ));
        }
        plan.registrations.push(registration);
    }
    Ok(plan)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Stage {
    Initialize,
    Commit,
    RemovePrevious,
    InstallCandidate,
    CleanupCandidate,
    RestorePrevious,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum NativeFailure {
    /// The native owner acknowledged that the requested mutation failed.
    Rejected(String),
    /// Delivery/response was lost. The mutation may have happened.
    Indeterminate(String),
}

impl From<&str> for NativeFailure {
    fn from(message: &str) -> Self {
        Self::Rejected(message.to_owned())
    }
}

impl From<String> for NativeFailure {
    fn from(message: String) -> Self {
        Self::Rejected(message)
    }
}

impl std::fmt::Display for NativeFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rejected(message) => formatter.write_str(message),
            Self::Indeterminate(message) => {
                write!(formatter, "Native ownership is uncertain: {message}")
            }
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct OperationError {
    pub stage: Stage,
    pub backend: KeyboardImplementation,
    pub registration: Option<Registration>,
    pub failure: NativeFailure,
}

#[derive(Debug, Default)]
pub(crate) struct BatchReport {
    pub changed: Vec<Registration>,
    pub retained: Vec<Registration>,
    pub errors: Vec<OperationError>,
}

#[derive(Debug, Default)]
pub(crate) struct SwitchReport {
    pub removal: BatchReport,
    pub installation: BatchReport,
    pub cleanup: BatchReport,
    pub restoration: BatchReport,
    /// Last acknowledged ownership, not a claim that uncertain operations had
    /// no effect. All indeterminate registrations remain separately recorded.
    pub owned: Vec<Registration>,
    pub uncertain: Vec<Registration>,
    pub initialization_error: Option<OperationError>,
}

impl SwitchReport {
    pub(crate) fn applied(&self) -> bool {
        self.initialization_error.is_none()
            && self.removal.errors.is_empty()
            && self.installation.errors.is_empty()
    }

    pub(crate) fn rollback_complete(&self) -> bool {
        !self.applied()
            && !self
                .initialization_error
                .as_ref()
                .is_some_and(|error| matches!(error.failure, NativeFailure::Indeterminate(_)))
            && self.uncertain.is_empty()
            && self.cleanup.errors.is_empty()
            && self.restoration.errors.is_empty()
    }
}

impl std::fmt::Display for SwitchReport {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(if self.rollback_complete() {
            "Keyboard switch rejected; previous configuration restored"
        } else {
            "Keyboard switch rejected; rollback incomplete or native ownership uncertain"
        })?;
        for error in self
            .initialization_error
            .iter()
            .chain(&self.removal.errors)
            .chain(&self.installation.errors)
            .chain(&self.cleanup.errors)
            .chain(&self.restoration.errors)
        {
            write!(formatter, "; {:?} {:?}", error.stage, error.backend)?;
            if let Some(registration) = &error.registration {
                write!(
                    formatter,
                    " binding '{}' ('{}')",
                    registration.binding.id, registration.binding.current_binding
                )?;
            }
            write!(formatter, ": {}", error.failure)?;
        }
        Ok(())
    }
}

/// Fold compatible roles while rejecting aliases belonging to different callbacks.
/// This must run before initialization or any native mutation.
pub(crate) fn fold_roles(entries: Vec<Registration>) -> Result<Vec<Registration>, String> {
    let mut folded: Vec<Registration> = Vec::new();
    for entry in entries {
        if let Some(owner) = folded
            .iter_mut()
            .find(|owner| owner.backend == entry.backend && owner.identity == entry.identity)
        {
            if owner.binding.id != entry.binding.id {
                return Err(format!(
                    "{:?} bindings '{}' and '{}' conflict on native identity",
                    entry.backend, owner.binding.id, entry.binding.id
                ));
            }
            for role in entry.roles {
                if !owner.roles.contains(&role) {
                    owner.roles.push(role);
                }
            }
        } else {
            if folded
                .iter()
                .any(|owner| owner.backend == entry.backend && owner.binding.id == entry.binding.id)
            {
                return Err(format!(
                    "{:?} callback '{}' has conflicting native identities",
                    entry.backend, entry.binding.id
                ));
            }
            folded.push(entry);
        }
    }
    Ok(folded)
}

/// Implementations must not hold a callback/status lock during native calls.
/// The outer exclusive permit is safe only when callbacks never wait for it.
pub(crate) trait NativeOperations {
    fn initialize(&mut self, backend: KeyboardImplementation) -> Result<(), NativeFailure>;
    fn remove(&mut self, registration: &Registration) -> Result<(), NativeFailure>;
    fn install(&mut self, registration: &Registration) -> Result<(), NativeFailure>;
}

fn record_error(
    batch: &mut BatchReport,
    uncertain: &mut Vec<Registration>,
    stage: Stage,
    registration: &Registration,
    failure: NativeFailure,
) {
    if matches!(failure, NativeFailure::Indeterminate(_)) {
        uncertain.push(registration.clone());
    }
    batch.errors.push(OperationError {
        stage,
        backend: registration.backend,
        registration: Some(registration.clone()),
        failure,
    });
}

fn forget(owned: &mut Vec<Registration>, registration: &Registration) {
    owned.retain(|entry| !entry.same_native_owner(registration));
}

/// Apply a fully validated desired native set. No settings/event callback is
/// accepted here: the caller can commit only after inspecting `applied()`.
/// Previous ownership must come from adapter snapshots, never from settings.
pub(crate) fn apply(
    native: &mut impl NativeOperations,
    candidate_backend: KeyboardImplementation,
    previous: &[Registration],
    desired: &[Registration],
) -> SwitchReport {
    let mut report = SwitchReport {
        owned: previous.to_vec(),
        ..SwitchReport::default()
    };
    if let Err(failure) = native.initialize(candidate_backend) {
        report.initialization_error = Some(OperationError {
            stage: Stage::Initialize,
            backend: candidate_backend,
            registration: None,
            failure,
        });
        return report;
    }

    for registration in previous {
        if desired
            .iter()
            .any(|entry| entry.same_native_owner(registration))
        {
            report.removal.retained.push(registration.clone());
            continue;
        }
        match native.remove(registration) {
            Ok(()) => {
                forget(&mut report.owned, registration);
                report.removal.changed.push(registration.clone());
            }
            Err(failure) => {
                report.removal.retained.push(registration.clone());
                record_error(
                    &mut report.removal,
                    &mut report.uncertain,
                    Stage::RemovePrevious,
                    registration,
                    failure,
                );
            }
        }
    }

    // Never install any candidates if even one required removal failed.
    if report.removal.errors.is_empty() {
        for registration in desired {
            if previous
                .iter()
                .any(|entry| entry.same_native_owner(registration))
            {
                report.installation.retained.push(registration.clone());
                continue;
            }
            match native.install(registration) {
                Ok(()) => {
                    report.owned.push(registration.clone());
                    report.installation.changed.push(registration.clone());
                }
                Err(failure) => record_error(
                    &mut report.installation,
                    &mut report.uncertain,
                    Stage::InstallCandidate,
                    registration,
                    failure,
                ),
            }
        }
    }

    if report.applied() {
        // Publish role transfers only after every native delta succeeds.
        report.owned = desired.to_vec();
        return report;
    }

    rollback(native, &mut report, previous);
    report
}

/// Undo an acknowledged application, including a rejected settings commit.
/// Continue through every cleanup/restoration failure and retain uncertainty.
pub(crate) fn rollback(
    native: &mut impl NativeOperations,
    report: &mut SwitchReport,
    previous: &[Registration],
) {
    for registration in report.installation.changed.iter().rev() {
        match native.remove(registration) {
            Ok(()) => {
                forget(&mut report.owned, registration);
                report.cleanup.changed.push(registration.clone());
            }
            Err(failure) => {
                report.cleanup.retained.push(registration.clone());
                record_error(
                    &mut report.cleanup,
                    &mut report.uncertain,
                    Stage::CleanupCandidate,
                    registration,
                    failure,
                );
            }
        }
    }
    // Restore only acknowledged removals, even after a cleanup/restoration error.
    for registration in report.removal.changed.iter().rev() {
        match native.install(registration) {
            Ok(()) => {
                report.owned.push(registration.clone());
                report.restoration.changed.push(registration.clone());
            }
            Err(failure) => record_error(
                &mut report.restoration,
                &mut report.uncertain,
                Stage::RestorePrevious,
                registration,
                failure,
            ),
        }
    }
    if report.rollback_complete() {
        report.owned = previous.to_vec();
    }
}

#[cfg(test)]
mod tests;
