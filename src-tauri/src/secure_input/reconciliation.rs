//! Platform-independent Carbon planning. No settings reads, locks or native calls.
use crate::settings::{KeyboardImplementation, ShortcutBinding};
use crate::shortcut::switch::{Registration, Role};
use std::collections::HashMap;

pub(crate) struct Intent<'a> {
    pub backend: KeyboardImplementation,
    pub bindings: &'a HashMap<String, ShortcutBinding>,
    pub post_process_enabled: bool,
    pub ready: bool,
    pub sustained: bool,
    pub cancel: bool,
    pub captured: bool,
}

#[derive(Debug, Default)]
pub(crate) struct Plan {
    pub registrations: Vec<Registration>,
    pub degraded: Vec<String>,
    pub uncovered: Vec<String>,
}

#[derive(Default)]
pub(crate) struct Coverage {
    pub registered: Vec<ShortcutBinding>,
    pub covered: Vec<String>,
    pub degraded: Vec<String>,
    pub uncovered: Vec<String>,
}

pub(crate) fn included(intent: &Intent<'_>, id: &str) -> bool {
    intent.ready
        && if id == "cancel" {
            intent.cancel && !cfg!(target_os = "linux")
        } else {
            !intent.captured
                && (id != "transcribe_with_post_process" || intent.post_process_enabled)
        }
}

/// Keep the existing Carbon conversion: mouse/modifier-only are immune, Fn
/// cannot be represented, and side-specific modifiers widen to their group.
pub(crate) fn plan(intent: &Intent<'_>) -> Plan {
    use handy_keys::Modifiers as M;
    let mut plan = Plan::default();
    if !intent.sustained || intent.backend != KeyboardImplementation::HandyKeys {
        return plan;
    }
    let mut ids: Vec<_> = intent.bindings.keys().collect();
    ids.sort();
    for id in ids {
        if !included(intent, id) {
            continue;
        }
        let binding = &intent.bindings[id];
        let Ok(hotkey) = binding.current_binding.parse::<handy_keys::Hotkey>() else {
            plan.uncovered.push(id.clone());
            continue;
        };
        match &hotkey.key {
            None => continue,
            Some(key) if key.to_string().to_lowercase().starts_with("mouse") => continue,
            _ => {}
        }
        if hotkey.modifiers.contains(M::FN) {
            plan.uncovered.push(id.clone());
            continue;
        }
        let mut widened = M::empty();
        let mut degraded = false;
        for group in [M::CTRL, M::OPT, M::SHIFT, M::CMD] {
            if hotkey.modifiers.intersects(group) {
                widened |= group;
                degraded |= !hotkey.modifiers.contains(group);
            }
        }
        let shadow = handy_keys::Hotkey::new(widened, hotkey.key)
            .ok()
            .and_then(|hotkey| {
                let mut shadow = binding.clone();
                shadow.current_binding = hotkey.to_handy_string();
                Registration::new(KeyboardImplementation::Tauri, shadow, Role::Shadow).ok()
            });
        match shadow {
            Some(shadow) => {
                if degraded {
                    plan.degraded.push(id.clone());
                }
                plan.registrations.push(shadow);
            }
            None => plan.uncovered.push(id.clone()),
        }
    }
    plan
}

/// Derive diagnostics from acknowledged ownership, never from attempted work.
/// Failed stale removals remain visible/owned, including when fallback stops.
pub(crate) fn coverage(
    plan: &Plan,
    owned: &[Registration],
    uncertain: &[Registration],
) -> Coverage {
    let mut result = Coverage {
        uncovered: plan.uncovered.clone(),
        ..Coverage::default()
    };
    for owner in owned
        .iter()
        .filter(|entry| entry.roles.contains(&Role::Shadow))
    {
        result.registered.push(owner.binding.clone());
        if !plan
            .registrations
            .iter()
            .any(|wanted| wanted.same_native_owner(owner))
        {
            result.uncovered.push(owner.binding.id.clone());
        }
    }
    for wanted in &plan.registrations {
        let id = &wanted.binding.id;
        if !owned.iter().any(|owner| owner.same_native_owner(wanted)) {
            result.uncovered.push(id.clone());
        } else if plan.degraded.contains(id) {
            result.degraded.push(id.clone());
        } else {
            result.covered.push(id.clone());
        }
    }
    for entry in uncertain {
        let id = &entry.binding.id;
        result.covered.retain(|covered| covered != id);
        result.degraded.retain(|degraded| degraded != id);
        result.uncovered.push(id.clone());
    }
    result.uncovered.sort();
    result.uncovered.dedup();
    result
}

#[cfg(test)]
mod tests;
