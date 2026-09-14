//! Startup uses the same acknowledged native delta as runtime switching.
use super::{runtime, switch};
use crate::settings::{self, KeyboardImplementation};
use runtime::CommandOperations;

pub(crate) fn initialize(operations: &mut impl CommandOperations) -> Result<(), String> {
    let backend = operations.settings().keyboard_implementation;
    match attempt(operations, backend) {
        Ok(()) => Ok(()),
        Err((error, clean)) if backend == KeyboardImplementation::HandyKeys && clean => {
            log::warn!("Shortcut startup failed; trying Tauri: {error}");
            attempt(operations, KeyboardImplementation::Tauri)
                .map_err(|(fallback, _)| format!("{error}; Tauri fallback failed: {fallback}"))
        }
        Err((error, _)) => Err(error),
    }
}

// The boolean permits fallback only after a fully acknowledged rollback.
fn attempt(
    operations: &mut impl CommandOperations,
    backend: KeyboardImplementation,
) -> Result<(), (String, bool)> {
    let original = operations.settings();
    let previous = operations.snapshot().map_err(|error| (error, false))?;
    let (cancel, captured, _, sustained) = operations.context().map_err(|error| (error, false))?;
    if captured {
        return Err(("Cannot initialize shortcuts during capture".into(), false));
    }
    let prepared = switch::prepare(
        backend,
        &original.bindings,
        &settings::get_default_settings().bindings,
        original.post_process_enabled,
    )
    .map_err(|error| (error, false))?;
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
        ready: true,
        sustained,
        cancel,
        captured,
    };
    let (desired, shadows) = runtime::desired_native_set(&explicit, prepared.registrations)
        .map_err(|error| (error, false))?;
    let mut report = runtime::apply_native_delta(operations, backend, &previous, &desired);
    if report.applied() {
        let current = operations.settings();
        let rejection = if !runtime::same_keyboard_configuration(&original, &current) {
            Some("Keyboard configuration changed during startup".into())
        } else {
            operations.publish(&report, &shadows).err()
        };
        if let Some(error) = rejection {
            report.initialization_error = Some(switch::OperationError {
                stage: switch::Stage::Commit,
                backend,
                registration: None,
                failure: switch::NativeFailure::Rejected(error),
            });
            switch::rollback(operations, &mut report, &previous);
        } else {
            let mut merged = current;
            merged.keyboard_implementation = backend;
            for binding in &prepared.resets {
                merged.bindings.insert(binding.id.clone(), binding.clone());
            }
            // In particular, never persist a fallback before its required set works.
            operations.write(merged);
            operations.emit(
                &prepared
                    .resets
                    .iter()
                    .map(|b| b.id.clone())
                    .collect::<Vec<_>>(),
            );
            return Ok(());
        }
    }
    let mut error = format!("Shortcut startup failed: {report}");
    let uncertain = !report.uncertain.is_empty()
        || report
            .initialization_error
            .as_ref()
            .is_some_and(|e| matches!(e.failure, switch::NativeFailure::Indeterminate(_)));
    let publication_failed = if let Err(publication) = operations.publish(&report, &shadows) {
        error.push_str(&format!("; ownership publication failed: {publication}"));
        true
    } else {
        false
    };
    if uncertain || publication_failed {
        operations.degraded(error.clone());
    }
    // A rejected cleanup leaves known owners in the adapters. The next attempt
    // snapshots and adopts those handles rather than registering them twice.
    Err((error, report.rollback_complete() && !publication_failed))
}
