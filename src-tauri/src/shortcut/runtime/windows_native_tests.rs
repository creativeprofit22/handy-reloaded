//! Real Windows registrations, keyboard hooks, and event-loop dispatch. Only
//! persistence and action execution are replaced: no user settings or microphone.
use super::*;
use crate::shortcut::handler::NativeEventProbe;
use enigo::{Direction, Enigo, Key, Keyboard};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

mod capture_ipc;

// tauri-build already compiles the app manifest, but only links it into
// application binaries. The native test executable also needs Common Controls
// v6 (TaskDialogIndirect); without this it fails in the Windows loader, before
// any test can run. Reuse the resource unchanged, only in this Windows test.
#[cfg(target_env = "msvc")]
#[link(
    name = "resource.lib",
    kind = "static",
    modifiers = "-bundle,+verbatim"
)]
extern "C" {}

#[link(name = "user32")]
extern "system" {
    fn RegisterHotKey(window: *mut std::ffi::c_void, id: i32, modifiers: u32, key: u32) -> i32;
    fn UnregisterHotKey(window: *mut std::ffi::c_void, id: i32) -> i32;
    fn GetAsyncKeyState(key: i32) -> i16;
}

struct Conflict;
impl Conflict {
    fn acquire() -> Self {
        // A thread-owned F23 registration competes with the plugin's hidden
        // window on the event-loop thread. This is a real OS conflict, not a
        // duplicate in Handy's ownership ledger.
        assert_ne!(
            unsafe { RegisterHotKey(std::ptr::null_mut(), 1, 0, 0x86) },
            0,
            "could not reserve F23 for the native conflict test: {}",
            std::io::Error::last_os_error()
        );
        Self
    }
}
impl Drop for Conflict {
    fn drop(&mut self) {
        // Created and dropped on the same worker thread, including on panic.
        let result = unsafe { UnregisterHotKey(std::ptr::null_mut(), 1) };
        assert_ne!(result, 0, "failed to release the test's F23 registration");
    }
}

struct DesktopHarness {
    app: AppHandle,
    current: settings::AppSettings,
    writes: usize,
    events: usize,
    degraded: Option<String>,
    cancel: bool,
}
impl NativeOperations for DesktopHarness {
    fn initialize(&mut self, backend: KeyboardImplementation) -> Result<(), NativeFailure> {
        Native(&self.app).initialize(backend)
    }
    fn remove(&mut self, entry: &Registration) -> Result<(), NativeFailure> {
        Native(&self.app).remove(entry)
    }
    fn install(&mut self, entry: &Registration) -> Result<(), NativeFailure> {
        Native(&self.app).install(entry)
    }
}
impl CommandOperations for DesktopHarness {
    fn stop_capture(&mut self) -> Result<(), String> {
        handy_keys::stop_recording_admitted(&self.app)
    }
    fn settings(&self) -> settings::AppSettings {
        self.current.clone()
    }
    fn snapshot(&self) -> Result<Vec<Registration>, String> {
        snapshot(&self.app)
    }
    fn context(&self) -> Result<(bool, bool, bool, bool), String> {
        Ok((self.cancel, false, true, false))
    }
    fn publish(
        &mut self,
        report: &switch::SwitchReport,
        shadows: &crate::secure_input::reconciliation::Plan,
    ) -> Result<(), String> {
        Command { app: &self.app }.publish(report, shadows)
    }
    fn write(&mut self, value: settings::AppSettings) {
        self.current = value;
        self.writes += 1;
    }
    fn emit(&mut self, resets: &[String]) {
        assert!(resets.is_empty());
        self.events += 1;
    }
    fn degraded(&mut self, message: String) {
        self.degraded = Some(message);
    }
}
impl DesktopHarness {
    fn capture(&mut self, suspended: bool) {
        let _permit = admit(&self.app).unwrap();
        let previous = self.snapshot().unwrap();
        let current = self.current.clone();
        let cancel = self.cancel;
        let (report, shadows) = capture_delta(self, &current, &previous, cancel, suspended, false).unwrap();
        assert!(report.applied(), "{report}");
        self.publish(&report, &shadows).unwrap();
        admission(&self.app).0.lock().unwrap().capture = suspended;
        if suspended {
            assert!(self.snapshot().unwrap().iter().all(|entry| entry.binding.id == "cancel"));
        }
    }

    fn request_cancel(&mut self, requested: bool) {
        let _permit = admit(&self.app).unwrap();
        self.cancel = requested;
        let backend = self.current.keyboard_implementation;
        let previous = self.snapshot().unwrap();
        let (desired, shadows) = desired_native_set(
            &crate::secure_input::reconciliation::Intent {
                backend,
                bindings: &self.current.bindings,
                post_process_enabled: true,
                ready: true,
                sustained: false,
                cancel: requested,
                captured: false,
            },
            previous.clone(),
        )
        .unwrap();
        let report = apply_native_delta(self, backend, &previous, &desired);
        assert!(report.applied(), "{report}");
        self.publish(&report, &shadows).unwrap();
        if !requested {
            assert!(self
                .snapshot()
                .unwrap()
                .iter()
                .all(|entry| entry.binding.id != "cancel"));
            // Check the OS, not just our ledger: idle Tauri must release F24.
            assert_ne!(
                unsafe { RegisterHotKey(std::ptr::null_mut(), 2, 0, 0x87) },
                0,
                "idle cancel still conflicts with a real Windows registration"
            );
            assert_ne!(unsafe { UnregisterHotKey(std::ptr::null_mut(), 2) }, 0);
        }
    }
}

impl Drop for DesktopHarness {
    fn drop(&mut self) {
        if let Ok(owned) = snapshot(&self.app) {
            for entry in owned {
                if let Err(error) = Native(&self.app).remove(&entry) {
                    eprintln!("native test cleanup failed: {error:?}");
                }
            }
        }
    }
}

fn assert_delivery(events: &Receiver<(String, bool)>, key: Key, id: &str) {
    assert!(
        events.try_recv().is_err(),
        "unexpected pending shortcut event"
    );
    // Do not disturb a physical modifier already held by the person at the PC.
    for modifier in [0x10, 0x11, 0x12, 0x5B, 0x5C] {
        assert_eq!(
            unsafe { GetAsyncKeyState(modifier) } & i16::MIN,
            0,
            "release keyboard modifiers before running the native test"
        );
    }
    let mut keyboard = Enigo::new(&enigo::Settings::default()).unwrap();
    keyboard.key(key, Direction::Press).unwrap();
    assert_eq!(
        events
            .recv_timeout(Duration::from_secs(5))
            .expect("native press did not reach Handy"),
        (id.into(), true)
    );
    keyboard.key(key, Direction::Release).unwrap();
    assert_eq!(
        events
            .recv_timeout(Duration::from_secs(5))
            .expect("native release did not reach Handy"),
        (id.into(), false)
    );
}

fn exercise(app: AppHandle, events: Receiver<(String, bool)>) {
    let mut current = settings::get_default_settings();
    current.keyboard_implementation = KeyboardImplementation::HandyKeys;
    current.post_process_enabled = true;
    for (id, binding) in &mut current.bindings {
        binding.current_binding = match id.as_str() {
            "transcribe" => "F22",
            "transcribe_with_post_process" => "F23",
            "cancel" => "F24",
            unexpected => panic!("assign an isolated native test key for {unexpected}"),
        }
        .into();
    }
    let original = serde_json::to_value(&current).unwrap();
    let mut h = DesktopHarness {
        app,
        current,
        writes: 0,
        events: 0,
        degraded: None,
        cancel: false,
    };
    let previous = switch::prepare(
        KeyboardImplementation::HandyKeys,
        &h.current.bindings,
        &settings::get_default_settings().bindings,
        true,
    )
    .unwrap()
    .registrations;
    h.initialize(KeyboardImplementation::HandyKeys).unwrap();
    for entry in &previous {
        h.install(entry).unwrap();
    }
    assert_delivery(&events, Key::F22, "transcribe");

    h.request_cancel(true);
    assert_delivery(&events, Key::F24, "cancel");
    let conflict = Conflict::acquire();
    let error = {
        let _permit = admit(&h.app).unwrap();
        orchestrate(&mut h, KeyboardImplementation::Tauri)
            .err()
            .expect("Windows must reject the occupied candidate")
    };
    assert!(error.contains("InstallCandidate"), "{error}");
    assert!(error.contains("transcribe_with_post_process"), "{error}");
    assert_eq!(serde_json::to_value(&h.current).unwrap(), original);
    assert_eq!((h.writes, h.events), (0, 0));
    assert!(h.degraded.is_none(), "{:?}", h.degraded);
    assert!(
        tauri_impl::snapshot(&h.app).unwrap().is_empty(),
        "partial F22 installation leaked"
    );
    let restored = h.snapshot().unwrap();
    assert_eq!(restored.len(), previous.len() + 1);
    assert_delivery(&events, Key::F24, "cancel");
    h.request_cancel(false);
    assert_delivery(&events, Key::F22, "transcribe");
    for entry in &previous {
        assert!(restored.iter().any(|owner| owner.same_native_owner(entry)));
    }
    // Verify actual key-down AND key-up delivery, not just ledger contents.
    assert_delivery(&events, Key::F22, "transcribe");
    assert_delivery(&events, Key::F23, "transcribe_with_post_process");
    println!("Windows native conflict rejected; partial candidate removed; both old shortcuts deliver press/release; settings unchanged. {error}");
    drop(conflict);

    for backend in [
        KeyboardImplementation::Tauri,
        KeyboardImplementation::HandyKeys,
    ] {
        h.request_cancel(true);
        assert_delivery(&events, Key::F24, "cancel");
        let result = {
            let _permit = admit(&h.app).unwrap();
            orchestrate(&mut h, backend).unwrap()
        };
        assert!(result.success);
        assert!(result.reset_bindings.is_empty());
        assert_eq!(h.current.keyboard_implementation, backend);
        assert_delivery(&events, Key::F24, "cancel");
        h.request_cancel(false);
        // No old hook may forward idle cancel. Send F24, then use the next
        // transcribe delivery as an event-loop barrier (no sleep-based check).
        let mut keyboard = Enigo::new(&enigo::Settings::default()).unwrap();
        keyboard.key(Key::F24, Direction::Click).unwrap();
        assert_delivery(&events, Key::F22, "transcribe");
        assert_delivery(&events, Key::F23, "transcribe_with_post_process");
        println!("Windows native switch to {backend:?}: both shortcuts deliver press/release.");
        for ending in ["success", "cancel"] {
            h.request_cancel(true);
            h.capture(true);
            keyboard.key(Key::F22, Direction::Click).unwrap();
            keyboard.key(Key::F23, Direction::Click).unwrap();
            // Cancel is the exempt event-loop barrier: no captured shortcut
            // may arrive ahead of it, but the dynamic cancel must still work.
            assert_delivery(&events, Key::F24, "cancel");
            h.capture(false);
            h.capture(false);
            assert_delivery(&events, Key::F22, "transcribe");
            assert_delivery(&events, Key::F23, "transcribe_with_post_process");
            h.request_cancel(false);
            println!("Windows native {backend:?} capture {ending}: no action dispatch during capture; original shortcuts restored, including repeated resume.");
        }
    }
    assert_eq!((h.writes, h.events), (2, 2));
    for entry in h.snapshot().unwrap() {
        h.remove(&entry).unwrap();
    }
    assert!(h.snapshot().unwrap().is_empty());

    // Every public cancel entrypoint must record intent synchronously under
    // the switch permit, rather than queueing an uncoordinated native job.
    let _permit = admit(&h.app).unwrap();
    for (start, stop) in [
        (
            tauri_impl::register_cancel_shortcut as fn(&AppHandle),
            tauri_impl::unregister_cancel_shortcut as fn(&AppHandle),
        ),
        (
            handy_keys::register_cancel_shortcut as fn(&AppHandle),
            handy_keys::unregister_cancel_shortcut as fn(&AppHandle),
        ),
        (
            crate::shortcut::register_cancel_shortcut as fn(&AppHandle),
            crate::shortcut::unregister_cancel_shortcut as fn(&AppHandle),
        ),
        (
            crate::secure_input::register_cancel_fallback as fn(&AppHandle),
            crate::secure_input::unregister_cancel_fallback as fn(&AppHandle),
        ),
    ] {
        start(&h.app);
        assert!(intent(&h.app).unwrap().0);
        stop(&h.app);
        assert!(!intent(&h.app).unwrap().0);
        assert!(h.snapshot().unwrap().is_empty());
    }
}

#[test]
fn windows_native_conflict_rolls_back_and_shortcuts_still_deliver() {
    let mut context = tauri::generate_context!();
    context.config_mut().app.windows.clear();
    context.config_mut().identifier = format!("com.handy.capture-test-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos());
    let app = tauri::Builder::default()
        .any_thread()
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .plugin(tauri_plugin_store::Builder::new().build())
        .register_uri_scheme_protocol("capture-test", |_, _| {
            tauri::http::Response::builder().header("Content-Type", "text/html").body(capture_ipc::HTML.as_bytes().to_vec()).unwrap()
        })
        .invoke_handler(tauri::generate_handler![
            crate::shortcut::handy_keys::start_handy_keys_recording,
            crate::shortcut::handy_keys::stop_handy_keys_recording,
            crate::shortcut::change_keyboard_implementation_setting,
            crate::shortcut::suspend_all_bindings,
            crate::shortcut::resume_all_bindings,
            capture_ipc::capture_checkpoint,
        ])
        .build(context)
        .expect("build windowless native Tauri test app");
    let (events_tx, events_rx) = mpsc::channel();
    app.manage(NativeEventProbe(events_tx));
    let (done_tx, done_rx) = mpsc::channel();
    let mut start = Some((events_rx, done_tx));
    // The event loop remains free while the worker waits on native callbacks.
    app.run_return(move |app, event| {
        if matches!(event, tauri::RunEvent::Ready) {
            let (events, done) = start.take().unwrap();
            let app = app.clone();
            std::thread::spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    exercise(app.clone(), events);
                    capture_ipc::exercise(&app);
                }));
                done.send(result).unwrap();
                app.exit(0);
            });
        }
    });
    if let Err(panic) = done_rx.recv().expect("native worker did not finish") {
        std::panic::resume_unwind(panic);
    }
}
