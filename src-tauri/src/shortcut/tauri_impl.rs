//! Tauri global-shortcut implementation
//!
//! This module provides shortcut functionality using Tauri's built-in
//! global-shortcut plugin.

use log::{debug, error, warn};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use tauri::{AppHandle, Manager};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};

use crate::settings::{self, ShortcutBinding};

use super::handler::handle_shortcut_event;
use super::switch::{NativeFailure, Registration, Role};

fn plugin_failure(error: tauri_plugin_global_shortcut::Error, message: String) -> NativeFailure {
    match error {
        tauri_plugin_global_shortcut::Error::GlobalHotkey(_) => NativeFailure::Rejected(message),
        // A dispatched task may have changed native state without a response.
        // Unknown future plugin errors are also conservatively indeterminate.
        _ => NativeFailure::Indeterminate(message),
    }
}

#[derive(Default)]
struct OwnershipLedger {
    busy: AtomicBool,
    registrations: Mutex<Vec<Registration>>,
    uncertain: Mutex<Vec<Registration>>,
}

struct LedgerPermit<'a>(&'a AtomicBool);

impl Drop for LedgerPermit<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl OwnershipLedger {
    fn enter(&self) -> Result<LedgerPermit<'_>, String> {
        self.busy
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .map_err(|_| "Tauri shortcut ownership is busy".to_string())?;
        Ok(LedgerPermit(&self.busy))
    }

    fn snapshot(&self) -> Result<Vec<Registration>, String> {
        let _permit = self.enter()?;
        if !self
            .uncertain
            .lock()
            .map_err(|_| "Tauri uncertainty lock poisoned".to_string())?
            .is_empty()
        {
            return Err(
                "Tauri native ownership is uncertain; a clean snapshot is unavailable".into(),
            );
        }
        let mut entries = self
            .registrations
            .lock()
            .map_err(|_| "Tauri shortcut ownership lock poisoned".to_string())?
            .clone();
        entries.sort_by(|left, right| left.binding.id.cmp(&right.binding.id));
        Ok(entries)
    }

    /// The status mutex is never held while the plugin waits on the main thread.
    /// Failed removal retains ownership; failed installation adds no ownership.
    fn mutate(
        &self,
        registration: Registration,
        install: bool,
        native: impl FnOnce() -> Result<(), NativeFailure>,
    ) -> Result<(), NativeFailure> {
        let _permit = self.enter()?;
        if self
            .uncertain
            .lock()
            .map_err(|_| NativeFailure::Indeterminate("Tauri uncertainty lock poisoned".into()))?
            .iter()
            .any(|entry| {
                entry.binding.id == registration.binding.id
                    || entry.identity == registration.identity
            })
        {
            return Err(NativeFailure::Indeterminate(format!(
                "Binding '{}' has unresolved native ownership",
                registration.binding.id
            )));
        }
        {
            let entries = self
                .registrations
                .lock()
                .map_err(|_| "Tauri shortcut ownership lock poisoned".to_string())?;
            if install {
                if entries.iter().any(|entry| {
                    entry.binding.id == registration.binding.id
                        || entry.identity == registration.identity
                }) {
                    return Err(NativeFailure::Rejected(format!(
                        "Binding '{}' conflicts with an owned Tauri shortcut",
                        registration.binding.id
                    )));
                }
            } else if !entries
                .iter()
                .any(|entry| entry.same_native_owner(&registration))
            {
                return Err(NativeFailure::Rejected(format!(
                    "Binding '{}' does not own this Tauri shortcut",
                    registration.binding.id
                )));
            }
        }
        if let Err(failure) = native() {
            if matches!(failure, NativeFailure::Indeterminate(_)) {
                self.uncertain
                    .lock()
                    .map_err(|_| {
                        NativeFailure::Indeterminate("Tauri uncertainty lock poisoned".into())
                    })?
                    .push(registration);
            }
            return Err(failure);
        }
        let mut entries = self.registrations.lock().map_err(|_| {
            NativeFailure::Indeterminate(
                "Native operation succeeded but Tauri ownership lock is poisoned".into(),
            )
        })?;
        if install {
            entries.push(registration);
        } else {
            entries.retain(|entry| !entry.same_native_owner(&registration));
        }
        Ok(())
    }
}

fn ledger(app: &AppHandle) -> tauri::State<'_, OwnershipLedger> {
    // manage is atomic: concurrent callers never replace an existing ledger.
    if app.try_state::<OwnershipLedger>().is_none() {
        app.manage(OwnershipLedger::default());
    }
    app.state::<OwnershipLedger>()
}

pub(crate) fn snapshot(app: &AppHandle) -> Result<Vec<Registration>, String> {
    ledger(app).snapshot()
}

/// Update role metadata only for acknowledged owners, without replacing handles.
pub(crate) fn publish_roles(app: &AppHandle, owned: &[Registration]) -> Result<(), String> {
    let ledger = ledger(app);
    let _permit = ledger.enter()?;
    let mut entries = ledger
        .registrations
        .lock()
        .map_err(|_| "Tauri ownership lock poisoned")?;
    for entry in entries.iter_mut() {
        if let Some(actual) = owned.iter().find(|actual| actual.same_native_owner(entry)) {
            entry.roles.clone_from(&actual.roles);
        }
    }
    Ok(())
}

#[cfg(test)]
mod ownership_tests {
    use super::*;

    fn registration(id: &str, text: &str) -> Registration {
        Registration::new(
            settings::KeyboardImplementation::Tauri,
            ShortcutBinding {
                id: id.into(),
                name: id.into(),
                description: String::new(),
                current_binding: text.into(),
                default_binding: text.into(),
            },
            Role::Primary,
        )
        .unwrap()
    }

    #[test]
    fn failed_install_is_not_owned_and_failed_removal_keeps_owner() {
        let ledger = OwnershipLedger::default();
        let entry = registration("a", "ctrl+a");
        assert!(ledger
            .mutate(entry.clone(), true, || Err("occupied".into()))
            .is_err());
        assert!(ledger.snapshot().unwrap().is_empty());
        ledger.mutate(entry.clone(), true, || Ok(())).unwrap();
        assert!(ledger
            .mutate(entry.clone(), false, || Err("native failure".into()))
            .is_err());
        assert!(ledger.snapshot().unwrap()[0].same_native_owner(&entry));
        ledger.mutate(entry, false, || Ok(())).unwrap();
        assert!(ledger.snapshot().unwrap().is_empty());
    }

    #[test]
    fn callback_and_native_identity_conflicts_are_rejected_before_native_call() {
        let ledger = OwnershipLedger::default();
        ledger
            .mutate(registration("a", "ctrl+a"), true, || Ok(()))
            .unwrap();
        for conflicting in [registration("a", "ctrl+b"), registration("b", "ctrl+a")] {
            assert!(ledger
                .mutate(conflicting, true, || panic!(
                    "conflict reached native plugin"
                ))
                .is_err());
        }
        assert!(ledger
            .mutate(registration("b", "ctrl+a"), false, || panic!(
                "must not remove another callback's shortcut"
            ))
            .is_err());
        assert_eq!(ledger.snapshot().unwrap().len(), 1);
    }

    #[test]
    fn lost_native_response_is_retained_and_blocks_false_clean_snapshot() {
        let ledger = OwnershipLedger::default();
        let entry = registration("a", "ctrl+a");
        assert!(matches!(
            ledger.mutate(entry.clone(), true, || Err(NativeFailure::Indeterminate(
                "lost response".into()
            ))),
            Err(NativeFailure::Indeterminate(_))
        ));
        assert!(ledger.snapshot().is_err());
        assert!(ledger.uncertain.lock().unwrap()[0].same_native_owner(&entry));
        assert!(ledger
            .mutate(entry, true, || panic!(
                "unresolved ownership must not be replaced"
            ))
            .is_err());
    }

    #[test]
    fn native_calls_hold_no_status_lock_and_nested_mutations_fail_without_waiting() {
        let ledger = OwnershipLedger::default();
        ledger
            .mutate(registration("a", "ctrl+a"), true, || {
                assert!(ledger.registrations.try_lock().is_ok());
                assert!(ledger.snapshot().is_err());
                assert!(ledger
                    .mutate(registration("b", "ctrl+b"), true, || panic!("overlap"))
                    .is_err());
                Ok(())
            })
            .unwrap();
        assert_eq!(ledger.snapshot().unwrap().len(), 1);
    }
}


/// Validate a shortcut string for the Tauri global-shortcut implementation.
/// Tauri requires at least one non-modifier key and doesn't support the fn key.
pub fn validate_shortcut(raw: &str) -> Result<(), String> {
    if raw.trim().is_empty() {
        return Err("Shortcut cannot be empty".into());
    }

    let modifiers = [
        "ctrl", "control", "shift", "alt", "option", "meta", "command", "cmd", "super", "win",
        "windows",
    ];

    // Check for fn key which Tauri doesn't support
    let parts: Vec<String> = raw.split('+').map(|p| p.trim().to_lowercase()).collect();
    for part in &parts {
        if part == "fn" || part == "function" {
            return Err("The 'fn' key is not supported by Tauri global shortcuts".into());
        }
    }

    // Check for at least one non-modifier key
    let has_non_modifier = parts.iter().any(|part| !modifiers.contains(&part.as_str()));

    if has_non_modifier {
        Ok(())
    } else {
        Err("Tauri shortcuts must include a main key (letter, number, F-key, etc.) in addition to modifiers".into())
    }
}

/// Register a shortcut using Tauri's global-shortcut plugin
pub fn register_shortcut(app: &AppHandle, binding: ShortcutBinding) -> Result<(), String> {
    register_report(app, binding).map_err(|error| error.to_string())
}

pub(crate) fn register_report(
    app: &AppHandle,
    binding: ShortcutBinding,
) -> Result<(), NativeFailure> {
    // Validate for Tauri requirements
    if let Err(e) = validate_shortcut(&binding.current_binding) {
        warn!(
            "register_tauri_shortcut validation error for binding '{}': {}",
            binding.current_binding, e
        );
        return Err(e.into());
    }

    // Parse shortcut and return error if it fails
    let shortcut = match binding.current_binding.parse::<Shortcut>() {
        Ok(s) => s,
        Err(e) => {
            let error_msg = format!(
                "Failed to parse shortcut '{}': {}",
                binding.current_binding, e
            );
            error!("register_tauri_shortcut parse error: {}", error_msg);
            return Err(error_msg.into());
        }
    };

    // Prevent duplicate registrations that would silently shadow one another
    if app.global_shortcut().is_registered(shortcut) {
        let error_msg = format!("Shortcut '{}' is already in use", binding.current_binding);
        warn!("register_tauri_shortcut duplicate error: {}", error_msg);
        return Err(error_msg.into());
    }

    // Clone binding.id for use in the closure
    let binding_id_for_closure = binding.id.clone();

    let registration = Registration::new(
        settings::KeyboardImplementation::Tauri,
        binding.clone(),
        if binding.id == "cancel" {
            Role::Cancel
        } else {
            Role::Primary
        },
    )?;
    ledger(app).mutate(registration, true, || {
        app.global_shortcut()
            .on_shortcut(shortcut, move |app_handle, scut, event| {
                if scut == &shortcut {
                    let shortcut_string = scut.into_string();
                    let is_pressed = event.state == ShortcutState::Pressed;
                    // Mirrors the handy-keys event log line; the distinct prefix
                    // makes it possible to tell which backend fired a shortcut
                    // (e.g. when diagnosing the Secure Input fallback)
                    debug!(
                        "tauri global-shortcut event: binding={}, shortcut={}, state={:?}",
                        binding_id_for_closure, shortcut_string, event.state
                    );
                    handle_shortcut_event(
                        app_handle,
                        &binding_id_for_closure,
                        &shortcut_string,
                        is_pressed,
                    );
                }
            })
            .map_err(|e| {
                let error_msg = format!(
                    "Couldn't register shortcut '{}': {}",
                    binding.current_binding, e
                );
                error!("register_tauri_shortcut registration error: {}", error_msg);
                plugin_failure(e, error_msg)
            })
    })?;

    Ok(())
}

/// Unregister a shortcut from Tauri's global-shortcut plugin
pub fn unregister_shortcut(app: &AppHandle, binding: ShortcutBinding) -> Result<(), String> {
    unregister_report(app, binding).map_err(|error| error.to_string())
}

pub(crate) fn unregister_report(
    app: &AppHandle,
    binding: ShortcutBinding,
) -> Result<(), NativeFailure> {
    let shortcut = match binding.current_binding.parse::<Shortcut>() {
        Ok(s) => s,
        Err(e) => {
            let error_msg = format!(
                "Failed to parse shortcut '{}' for unregistration: {}",
                binding.current_binding, e
            );
            error!("unregister_tauri_shortcut parse error: {}", error_msg);
            return Err(error_msg.into());
        }
    };

    let registration = Registration::new(
        settings::KeyboardImplementation::Tauri,
        binding.clone(),
        if binding.id == "cancel" {
            Role::Cancel
        } else {
            Role::Primary
        },
    )?;
    ledger(app).mutate(registration, false, || {
        app.global_shortcut().unregister(shortcut).map_err(|e| {
            let error_msg = format!(
                "Failed to unregister shortcut '{}': {}",
                binding.current_binding, e
            );
            error!("unregister_tauri_shortcut error: {}", error_msg);
            plugin_failure(e, error_msg)
        })
    })?;

    Ok(())
}

/// Request cancel through the shared lifecycle, never a backend-local async job.
/// Ownership and Linux exclusion are resolved by the admitted reconciler.
pub fn register_cancel_shortcut(app: &AppHandle) {
    super::register_cancel_shortcut(app);
}

/// Remove cancel from its actual owner, even after a backend switch.
pub fn unregister_cancel_shortcut(app: &AppHandle) {
    super::unregister_cancel_shortcut(app);
}
