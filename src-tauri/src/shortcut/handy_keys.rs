//! Handy-keys based keyboard shortcut implementation
//!
//! This module provides an alternative to Tauri's global-shortcut plugin
//! using the handy-keys library for more control over keyboard events.
//!
//! ## Architecture
//!
//! The implementation uses a dedicated manager thread that owns the `HotkeyManager`:
//!
//! ```text
//! ┌─────────────────┐     commands      ┌──────────────────────┐
//! │   Main Thread   │ ───────────────▶ │   Manager Thread     │
//! │                 │   (via channel)   │                      │
//! │ - register()    │                   │ - owns HotkeyManager │
//! │ - unregister()  │                   │ - polls for events   │
//! └─────────────────┘                   │ - dispatches actions │
//!                                       └──────────────────────┘
//! ```
//!
//! This design ensures thread-safety since `HotkeyManager` is only accessed
//! from a single thread. Commands (register/unregister) are sent via an mpsc
//! channel and responses are synchronously awaited.
//!
//! ## Recording Mode
//!
//! For UI key capture, a separate `KeyboardListener` is created on-demand and
//! polled from a dedicated recording thread. Events are emitted to the frontend
//! via Tauri's event system.

use handy_keys::{Hotkey, HotkeyId, HotkeyManager, HotkeyState, KeyboardListener};
use log::{debug, error, info};
use serde::Serialize;
use specta::Type;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use tauri::{AppHandle, Emitter, Manager};

use crate::settings::{self, get_settings, ShortcutBinding};

use super::handler::handle_shortcut_event;

/// Actual manager ownership, never reconstructed from the selected settings.
pub(crate) fn snapshot(app: &AppHandle) -> Result<Vec<super::switch::Registration>, String> {
    use super::switch::{Registration, Role};
    let Some(state) = app.try_state::<HandyKeysState>() else {
        return Ok(Vec::new());
    };
    state
        .snapshot()
        .map_err(|error| error.to_string())?
        .into_iter()
        .map(|(id, text)| {
            let role = if id == "cancel" {
                Role::Cancel
            } else {
                Role::Primary
            };
            Registration::new(
                settings::KeyboardImplementation::HandyKeys,
                ShortcutBinding {
                    name: id.clone(),
                    description: String::new(),
                    id,
                    current_binding: text.clone(),
                    default_binding: text,
                },
                role,
            )
        })
        .collect()
}

/// Commands that can be sent to the hotkey manager thread
enum ManagerCommand {
    Register {
        binding_id: String,
        hotkey_string: String,
        response: Sender<Result<(), String>>,
    },
    Unregister {
        binding_id: String,
        response: Sender<Result<(), String>>,
    },
    Snapshot {
        response: Sender<Vec<(String, String)>>,
    },
    Shutdown,
}

/// State for the handy-keys shortcut manager
pub struct HandyKeysState {
    /// Channel to send commands to the manager thread (wrapped in Mutex for Sync)
    command_sender: Mutex<Sender<ManagerCommand>>,
    /// Handle to the manager thread (wrapped in Mutex for Sync, allows proper join on drop)
    thread_handle: Mutex<Option<JoinHandle<()>>>,
    /// Recording listener for UI key capture (only active during recording)
    recording_listener: Mutex<Option<KeyboardListener>>,
    /// Capture worker and its suspension debt, independent of the active backend.
    capture: Mutex<super::capture::Session>,
    uncertain: Mutex<Vec<ShortcutBinding>>,
}

/// Key event sent to frontend during recording mode
#[derive(Debug, Clone, Serialize, Type)]
pub struct FrontendKeyEvent {
    /// Currently pressed modifier keys
    pub modifiers: Vec<String>,
    /// The key that was pressed (if any)
    pub key: Option<String>,
    /// Whether this is a key down event
    pub is_key_down: bool,
    /// The full hotkey string (e.g., "option+space")
    pub hotkey_string: String,
}

// The native manager is created and retained on its owning thread; T need not
// be Send. Only readiness and commands cross the thread boundary.
fn start_manager_thread<T: 'static>(
    initialize: impl FnOnce() -> Result<T, String> + Send + 'static,
    run: impl FnOnce(T) + Send + 'static,
) -> Result<JoinHandle<()>, String> {
    let (ready, readiness) = mpsc::channel();
    let thread = thread::Builder::new()
        .name("handy-keys-manager".into())
        .spawn(move || match initialize() {
            Ok(manager) => {
                if ready.send(Ok(())).is_ok() {
                    run(manager);
                }
            }
            Err(error) => {
                let _ = ready.send(Err(error));
            }
        })
        .map_err(|e| format!("Failed to start shortcut manager thread: {e}"))?;
    match readiness.recv() {
        Ok(Ok(())) => Ok(thread),
        result => {
            let _ = thread.join();
            Err(match result {
                Ok(Err(error)) => error,
                _ => "Shortcut manager stopped before reporting readiness".into(),
            })
        }
    }
}

#[cfg(test)]
mod readiness_tests {
    use super::*;

    #[test]
    fn initialization_failure_is_returned_to_the_switch_owner() {
        let result = start_manager_thread::<()>(
            || Err("native backend unavailable".into()),
            |_| panic!("failed initialization must never enter the event loop"),
        );
        // Join even under the old behavior so a failed regression leaves no thread.
        let error = match result {
            Ok(thread) => {
                thread.join().unwrap();
                None
            }
            Err(error) => Some(error),
        };
        assert_eq!(error.as_deref(), Some("native backend unavailable"));
    }

    #[test]
    fn initialization_panic_is_not_reported_as_success() {
        let result = start_manager_thread::<()>(
            || panic!("injected startup panic"),
            |_| panic!("must not run"),
        );
        match result {
            Ok(thread) => {
                let _ = thread.join();
                panic!("startup reported success");
            }
            Err(error) => assert!(error.contains("before reporting readiness")),
        }
    }

    #[test]
    fn native_state_is_created_and_used_on_the_same_thread() {
        let (sender, receiver) = mpsc::channel();
        let thread = start_manager_thread(
            || Ok((std::rc::Rc::new(42), thread::current().id())),
            move |(value, owner)| {
                sender
                    .send((*value, owner == thread::current().id()))
                    .unwrap();
            },
        )
        .unwrap();
        assert_eq!(receiver.recv().unwrap(), (42, true));
        thread.join().unwrap();
    }
}

/// Keep both ownership indexes unchanged unless the native mutation succeeds.
fn register_owned<H: Copy + Eq + std::hash::Hash>(
    forward: &mut HashMap<String, H>,
    reverse: &mut HashMap<H, (String, String)>,
    binding_id: &str,
    hotkey_string: &str,
    native: impl FnOnce() -> Result<H, String>,
) -> Result<(), String> {
    if forward.contains_key(binding_id) {
        return Err(format!(
            "Binding '{binding_id}' already owns a native shortcut"
        ));
    }
    let handle = native()?;
    forward.insert(binding_id.to_owned(), handle);
    reverse.insert(handle, (binding_id.to_owned(), hotkey_string.to_owned()));
    Ok(())
}

fn unregister_owned<H: Copy + Eq + std::hash::Hash>(
    forward: &mut HashMap<String, H>,
    reverse: &mut HashMap<H, (String, String)>,
    binding_id: &str,
    native: impl FnOnce(H) -> Result<(), String>,
) -> Result<(), String> {
    if let Some(handle) = forward.get(binding_id).copied() {
        native(handle)?;
        forward.remove(binding_id);
        reverse.remove(&handle);
    }
    Ok(())
}

#[cfg(test)]
mod ownership_tests {
    use super::*;

    #[test]
    fn failed_removal_keeps_both_indexes_and_can_be_retried() {
        let mut forward = HashMap::new();
        let mut reverse = HashMap::new();
        register_owned(&mut forward, &mut reverse, "a", "ctrl+a", || Ok(7_u32)).unwrap();
        assert!(unregister_owned(&mut forward, &mut reverse, "a", |_| Err(
            "native failure".into()
        ))
        .is_err());
        assert_eq!(forward.get("a"), Some(&7));
        assert_eq!(reverse.get(&7), Some(&("a".into(), "ctrl+a".into())));
        unregister_owned(&mut forward, &mut reverse, "a", |handle| {
            assert_eq!(handle, 7);
            Ok(())
        })
        .unwrap();
        assert!(forward.is_empty());
        assert!(reverse.is_empty());
    }

    #[test]
    fn duplicate_callback_id_never_calls_native_or_replaces_handle() {
        let mut forward = HashMap::new();
        let mut reverse = HashMap::new();
        register_owned(&mut forward, &mut reverse, "a", "ctrl+a", || Ok(7_u32)).unwrap();
        assert!(
            register_owned(&mut forward, &mut reverse, "a", "ctrl+b", || panic!(
                "must reject before native registration"
            ))
            .is_err()
        );
        assert_eq!(forward.get("a"), Some(&7));
        assert_eq!(reverse.len(), 1);
        assert_eq!(reverse[&7].1, "ctrl+a");
    }

    #[test]
    fn rejected_installation_changes_neither_index() {
        let mut forward = HashMap::<String, u32>::new();
        let mut reverse = HashMap::new();
        assert!(
            register_owned(&mut forward, &mut reverse, "a", "ctrl+a", || Err(
                "occupied".into()
            ))
            .is_err()
        );
        assert!(forward.is_empty());
        assert!(reverse.is_empty());
    }
}

impl HandyKeysState {
    /// Create a new HandyKeysState
    pub fn new(app: AppHandle) -> Result<Self, String> {
        let (cmd_tx, cmd_rx) = mpsc::channel::<ManagerCommand>();

        // Start the manager thread
        let app_clone = app.clone();
        let thread_handle = start_manager_thread(
            || HotkeyManager::new_with_blocking().map_err(|e| e.to_string()),
            move |manager| Self::manager_thread(cmd_rx, app_clone, manager),
        )?;

        Ok(Self {
            command_sender: Mutex::new(cmd_tx),
            thread_handle: Mutex::new(Some(thread_handle)),
            recording_listener: Mutex::new(None),
            capture: Mutex::new(super::capture::Session::default()),
            uncertain: Mutex::new(Vec::new()),
        })
    }

    /// The main manager thread - owns the HotkeyManager and processes commands
    fn manager_thread(cmd_rx: Receiver<ManagerCommand>, app: AppHandle, manager: HotkeyManager) {
        info!("handy-keys manager thread started");

        // Maps binding IDs to HotkeyIds and hotkey strings
        let mut binding_to_hotkey: HashMap<String, HotkeyId> = HashMap::new();
        let mut hotkey_to_binding: HashMap<HotkeyId, (String, String)> = HashMap::new(); // (binding_id, hotkey_string)

        loop {
            // Check for hotkey events (non-blocking)
            while let Some(event) = manager.try_recv() {
                if let Some((binding_id, hotkey_string)) = hotkey_to_binding.get(&event.id) {
                    debug!(
                        "handy-keys event: binding={}, hotkey={}, state={:?}",
                        binding_id, hotkey_string, event.state
                    );
                    let is_pressed = event.state == HotkeyState::Pressed;
                    handle_shortcut_event(&app, binding_id, hotkey_string, is_pressed);
                }
            }

            // Check for commands (non-blocking with timeout)
            match cmd_rx.recv_timeout(std::time::Duration::from_millis(10)) {
                Ok(cmd) => match cmd {
                    ManagerCommand::Register {
                        binding_id,
                        hotkey_string,
                        response,
                    } => {
                        let result = Self::do_register(
                            &manager,
                            &mut binding_to_hotkey,
                            &mut hotkey_to_binding,
                            &binding_id,
                            &hotkey_string,
                        );
                        let _ = response.send(result);
                    }
                    ManagerCommand::Unregister {
                        binding_id,
                        response,
                    } => {
                        let result = Self::do_unregister(
                            &manager,
                            &mut binding_to_hotkey,
                            &mut hotkey_to_binding,
                            &binding_id,
                        );
                        let _ = response.send(result);
                    }
                    ManagerCommand::Snapshot { response } => {
                        let mut bindings: Vec<_> = hotkey_to_binding.values().cloned().collect();
                        bindings.sort_by(|left, right| left.0.cmp(&right.0));
                        let _ = response.send(bindings);
                    }
                    ManagerCommand::Shutdown => {
                        info!("handy-keys manager thread shutting down");
                        break;
                    }
                },
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    // No command, continue
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    info!("Command channel disconnected, shutting down");
                    break;
                }
            }
        }

        info!("handy-keys manager thread stopped");
    }

    /// Register a hotkey
    fn do_register(
        manager: &HotkeyManager,
        binding_to_hotkey: &mut HashMap<String, HotkeyId>,
        hotkey_to_binding: &mut HashMap<HotkeyId, (String, String)>,
        binding_id: &str,
        hotkey_string: &str,
    ) -> Result<(), String> {
        let hotkey: Hotkey = hotkey_string
            .parse()
            .map_err(|e| format!("Failed to parse hotkey '{}': {}", hotkey_string, e))?;

        register_owned(
            binding_to_hotkey,
            hotkey_to_binding,
            binding_id,
            hotkey_string,
            || {
                manager
                    .register(hotkey)
                    .map_err(|e| format!("Failed to register hotkey: {e}"))
            },
        )?;

        debug!(
            "Registered handy-keys shortcut: {} -> {:?}",
            binding_id, hotkey
        );
        Ok(())
    }

    /// Unregister a hotkey
    fn do_unregister(
        manager: &HotkeyManager,
        binding_to_hotkey: &mut HashMap<String, HotkeyId>,
        hotkey_to_binding: &mut HashMap<HotkeyId, (String, String)>,
        binding_id: &str,
    ) -> Result<(), String> {
        unregister_owned(binding_to_hotkey, hotkey_to_binding, binding_id, |id| {
            manager
                .unregister(id)
                .map_err(|e| format!("Failed to unregister hotkey: {e}"))
        })
    }

    /// Register a shortcut binding
    pub fn register(&self, binding: &ShortcutBinding) -> Result<(), String> {
        self.register_report(binding)
            .map_err(|error| error.to_string())
    }

    pub(crate) fn register_report(
        &self,
        binding: &ShortcutBinding,
    ) -> Result<(), super::switch::NativeFailure> {
        use super::switch::NativeFailure;
        if !self
            .uncertain
            .lock()
            .map_err(|_| {
                NativeFailure::Indeterminate("HandyKeys uncertainty lock poisoned".into())
            })?
            .is_empty()
        {
            return Err(NativeFailure::Indeterminate(
                "HandyKeys has unresolved native ownership; restart required".into(),
            ));
        }
        let (tx, rx) = mpsc::channel();
        self.command_sender
            .lock()
            .map_err(|_| NativeFailure::Rejected("Failed to lock command_sender".into()))?
            .send(ManagerCommand::Register {
                binding_id: binding.id.clone(),
                hotkey_string: binding.current_binding.clone(),
                response: tx,
            })
            .map_err(|_| NativeFailure::Rejected("Failed to send register command".into()))?;

        rx.recv()
            .map_err(|_| {
                if let Ok(mut uncertain) = self.uncertain.lock() {
                    uncertain.push(binding.clone());
                }
                NativeFailure::Indeterminate("Failed to receive register response".into())
            })?
            .map_err(NativeFailure::Rejected)
    }

    /// Unregister a shortcut binding
    pub fn unregister(&self, binding: &ShortcutBinding) -> Result<(), String> {
        self.unregister_report(binding)
            .map_err(|error| error.to_string())
    }

    pub(crate) fn unregister_report(
        &self,
        binding: &ShortcutBinding,
    ) -> Result<(), super::switch::NativeFailure> {
        use super::switch::NativeFailure;
        if !self
            .uncertain
            .lock()
            .map_err(|_| {
                NativeFailure::Indeterminate("HandyKeys uncertainty lock poisoned".into())
            })?
            .is_empty()
        {
            return Err(NativeFailure::Indeterminate(
                "HandyKeys has unresolved native ownership; restart required".into(),
            ));
        }
        let (tx, rx) = mpsc::channel();
        self.command_sender
            .lock()
            .map_err(|_| NativeFailure::Rejected("Failed to lock command_sender".into()))?
            .send(ManagerCommand::Unregister {
                binding_id: binding.id.clone(),
                response: tx,
            })
            .map_err(|_| NativeFailure::Rejected("Failed to send unregister command".into()))?;

        rx.recv()
            .map_err(|_| {
                if let Ok(mut uncertain) = self.uncertain.lock() {
                    uncertain.push(binding.clone());
                }
                NativeFailure::Indeterminate("Failed to receive unregister response".into())
            })?
            .map_err(NativeFailure::Rejected)
    }

    /// Snapshot actual manager-owned registrations, ordered by callback ID.
    /// A lost response is not equivalent to an empty registration set.
    pub(crate) fn snapshot(&self) -> Result<Vec<(String, String)>, super::switch::NativeFailure> {
        use super::switch::NativeFailure;
        if !self
            .uncertain
            .lock()
            .map_err(|_| {
                NativeFailure::Indeterminate("HandyKeys uncertainty lock poisoned".into())
            })?
            .is_empty()
        {
            return Err(NativeFailure::Indeterminate(
                "HandyKeys has unresolved native ownership; restart required".into(),
            ));
        }
        let (tx, rx) = mpsc::channel();
        self.command_sender
            .lock()
            .map_err(|_| NativeFailure::Indeterminate("Shortcut manager sender poisoned".into()))?
            .send(ManagerCommand::Snapshot { response: tx })
            .map_err(|_| {
                NativeFailure::Indeterminate("Shortcut manager unavailable for snapshot".into())
            })?;
        rx.recv().map_err(|_| {
            NativeFailure::Indeterminate("Shortcut manager snapshot response lost".into())
        })
    }

    /// Caller holds shortcut admission. Native construction/disposal stays on
    /// the command path; only polling runs on the session-specific worker.
    fn start_recording(&self, app: &AppHandle) -> Result<(), String> {
        self.capture
            .lock()
            .map_err(|_| "Capture session lock poisoned")?
            .start(
                || {
                    if super::runtime::intent(app)?.1 {
                        return Err("Another shortcut editor owns capture suspension".into());
                    }
                    super::runtime::capture_admitted(app, true)
                },
                || {
                    let listener = KeyboardListener::new()
                        .map_err(|e| format!("Failed to create keyboard listener: {e}"))?;
                    *self
                        .recording_listener
                        .lock()
                        .map_err(|_| "Failed to lock recording_listener")? = Some(listener);
                    let app = app.clone();
                    match super::capture::Worker::spawn(move |running| {
                        Self::recording_loop(app, running)
                    }) {
                        Ok(worker) => Ok(worker),
                        Err(error) => {
                            self.dispose_recording_listener()?;
                            Err(error)
                        }
                    }
                },
                || super::runtime::capture_admitted(app, false),
            )
    }

    /// Recording loop - emits key events to frontend during recording
    fn recording_loop(app: AppHandle, running: Arc<AtomicBool>) {
        while running.load(Ordering::SeqCst) {
            let event = {
                let state = match app.try_state::<HandyKeysState>() {
                    Some(s) => s,
                    None => break,
                };
                let listener = state.recording_listener.lock().ok();
                listener.as_ref().and_then(|l| l.as_ref()?.try_recv())
            };

            if let Some(key_event) = event {
                // Convert to frontend-friendly format
                let frontend_event = FrontendKeyEvent {
                    modifiers: modifiers_to_strings(key_event.modifiers),
                    key: key_event.key.map(|k| k.to_string().to_lowercase()),
                    is_key_down: key_event.is_key_down,
                    hotkey_string: key_event
                        .as_hotkey()
                        .map(|h| h.to_handy_string())
                        .unwrap_or_default(),
                };

                // Emit to frontend
                if let Err(e) = app.emit("handy-keys-event", &frontend_event) {
                    error!("Failed to emit key event: {}", e);
                }
            } else {
                thread::sleep(std::time::Duration::from_millis(10));
            }
        }

        debug!("Recording loop ended");
    }

    fn dispose_recording_listener(&self) -> Result<(), String> {
        *self
            .recording_listener
            .lock()
            .map_err(|_| "Failed to lock recording_listener")? = None;
        Ok(())
    }

    /// Join without holding the listener mutex needed by the worker, then
    /// restore only the suspension acquired by this HandyKeys session.
    fn stop_recording(&self, app: &AppHandle) -> Result<(), String> {
        self.capture
            .lock()
            .map_err(|_| "Capture session lock poisoned")?
            .stop(
                || self.dispose_recording_listener(),
                || super::runtime::capture_admitted(app, false),
            )
    }
}

impl Drop for HandyKeysState {
    fn drop(&mut self) {
        // Join capture before dropping its native listener. No listener lock is
        // held here and the worker never takes the capture-session lock.
        if let Ok(capture) = self.capture.get_mut() {
            if let Err(error) = capture.stop(|| Ok(()), || Ok(())) {
                error!("Failed to stop capture on shutdown: {error}");
            }
        }

        // Send shutdown command
        if let Ok(sender) = self.command_sender.lock() {
            let _ = sender.send(ManagerCommand::Shutdown);
        }

        // Wait for the manager thread to finish
        if let Ok(mut handle) = self.thread_handle.lock() {
            if let Some(h) = handle.take() {
                let _ = h.join();
            }
        }
    }
}

/// Convert handy-keys Modifiers to a list of strings
fn modifiers_to_strings(modifiers: handy_keys::Modifiers) -> Vec<String> {
    let mut result = Vec::new();

    if modifiers.contains(handy_keys::Modifiers::CTRL) {
        result.push("ctrl".to_string());
    }
    if modifiers.contains(handy_keys::Modifiers::OPT) {
        #[cfg(target_os = "macos")]
        result.push("option".to_string());
        #[cfg(not(target_os = "macos"))]
        result.push("alt".to_string());
    }
    if modifiers.contains(handy_keys::Modifiers::SHIFT) {
        result.push("shift".to_string());
    }
    if modifiers.contains(handy_keys::Modifiers::CMD) {
        #[cfg(target_os = "macos")]
        result.push("command".to_string());
        #[cfg(not(target_os = "macos"))]
        result.push("super".to_string());
    }
    if modifiers.contains(handy_keys::Modifiers::FN) {
        result.push("fn".to_string());
    }

    result
}

/// Validate a shortcut string for the HandyKeys implementation.
/// HandyKeys is more permissive: allows modifier-only combos and the fn key.
pub fn validate_shortcut(raw: &str) -> Result<(), String> {
    if raw.trim().is_empty() {
        return Err("Shortcut cannot be empty".into());
    }
    // HandyKeys accepts modifier-only, key-only, and modifier+key combos
    // Just verify the string is parseable
    raw.parse::<Hotkey>()
        .map(|_| ())
        .map_err(|e| format!("Invalid shortcut for HandyKeys: {}", e))
}

/// Initialize the backend without registering bindings or writing settings.
/// Runtime switching uses this before removing the previous registrations.
pub(super) fn ensure_initialized(app: &AppHandle) -> Result<(), String> {
    if app.try_state::<HandyKeysState>().is_none() {
        let state = HandyKeysState::new(app.clone())?;
        app.manage(state);
    }
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

/// Register a shortcut
pub fn register_shortcut(app: &AppHandle, binding: ShortcutBinding) -> Result<(), String> {
    let state = app
        .try_state::<HandyKeysState>()
        .ok_or("HandyKeysState not initialized")?;
    state.register(&binding)
}

/// Unregister a shortcut
pub fn unregister_shortcut(app: &AppHandle, binding: ShortcutBinding) -> Result<(), String> {
    let state = app
        .try_state::<HandyKeysState>()
        .ok_or("HandyKeysState not initialized")?;
    state.unregister(&binding)
}

/// Start key recording mode
#[tauri::command]
#[specta::specta]
pub fn start_handy_keys_recording(app: AppHandle, binding_id: String) -> Result<(), String> {
    let _permit = super::runtime::admit(&app)?;
    let settings = get_settings(&app);
    if settings.keyboard_implementation != settings::KeyboardImplementation::HandyKeys {
        return Err("handy-keys is not the active keyboard implementation".into());
    }

    // While Secure Input is active the tap receives no KeyDown/KeyUp, so the
    // recorder would silently capture just the modifier and overwrite the
    // binding with it (issue #1578). Refuse instead; the frontend maps this
    // marker to a localized explanation, and the noted impact makes the
    // warning banner appear with the full story.
    if crate::secure_input::is_enabled_now() {
        crate::secure_input::note_recorder_blocked(&app);
        return Err("secure-input-active".into());
    }

    let state = app
        .try_state::<HandyKeysState>()
        .ok_or("HandyKeysState not initialized")?;

    if !settings.bindings.contains_key(&binding_id) {
        return Err("Unknown shortcut binding".into());
    }
    state.start_recording(&app)
}

/// Stop key recording mode
#[tauri::command]
#[specta::specta]
pub fn stop_handy_keys_recording(app: AppHandle) -> Result<(), String> {
    // A completed switch has already disposed this session. In particular, a
    // stale editor's unmount must not touch a new Tauri editor's suspension.
    if !owns_capture_suspension(&app)? {
        return Ok(());
    }
    let _permit = super::runtime::admit_cleanup(&app)?;
    stop_recording_admitted(&app)
}

#[cfg(all(test, target_os = "windows"))]
pub(crate) fn assert_capture_stopped(app: &AppHandle) {
    let state = app.state::<HandyKeysState>();
    let capture = state.capture.lock().unwrap();
    assert!(!capture.owns_suspension());
    assert!(!capture.has_worker());
    assert!(state.recording_listener.lock().unwrap().is_none());
}

pub(crate) fn owns_capture_suspension(app: &AppHandle) -> Result<bool, String> {
    match app.try_state::<HandyKeysState>() {
        // Public stop can reach this before admission. Never block the IPC
        // thread behind a switch whose native work may need that same thread.
        Some(state) => Ok(state
            .capture
            .try_lock()
            .map_err(|error| match error {
                std::sync::TryLockError::WouldBlock => {
                    "Keyboard operation busy; retry when it completes"
                }
                std::sync::TryLockError::Poisoned(_) => "Capture session lock poisoned",
            })?
            .owns_suspension()),
        None => Ok(false),
    }
}

/// Used by public stop and the admitted switch transaction before taking its
/// registration snapshot. No selected-backend check belongs on termination.
pub(crate) fn stop_recording_admitted(app: &AppHandle) -> Result<(), String> {
    match app.try_state::<HandyKeysState>() {
        Some(state) => state.stop_recording(app),
        None => Ok(()),
    }
}
