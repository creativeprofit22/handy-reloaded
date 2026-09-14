//! Hidden, real WebView IPC against the production capture and switch commands.
//! Uses a test-only application identifier/store and the existing action probe.
use super::*;
use tauri::Listener;
use tauri_plugin_store::StoreExt;

pub(super) const HTML: &str = r#"<!doctype html><html><body><script>
window.addEventListener('DOMContentLoaded', async () => {
  const invoke = window.__TAURI_INTERNALS__.invoke;
  const checkpoint = stage => invoke('capture_checkpoint', { stage });
  try {
    await invoke('stop_handy_keys_recording');
    await invoke('start_handy_keys_recording', { bindingId: 'transcribe' });
    await checkpoint('recording');
    await invoke('change_keyboard_implementation_setting', { implementation: 'tauri' });
    await invoke('stop_handy_keys_recording');
    await invoke('stop_handy_keys_recording');
    await checkpoint('stopped');
    await invoke('suspend_all_bindings');
    await invoke('stop_handy_keys_recording');
    await checkpoint('tauri-editor');
    await invoke('resume_all_bindings');
    await invoke('change_keyboard_implementation_setting', { implementation: 'handy_keys' });
    await invoke('start_handy_keys_recording', { bindingId: 'transcribe' });
    await checkpoint('restarted');
    await invoke('stop_handy_keys_recording');
    await checkpoint('finished');
  } catch (error) {
    await checkpoint('error: ' + String(error));
  }
});
</script></body></html>"#;

pub(super) struct Checkpoints(mpsc::Sender<(String, mpsc::Sender<()>)>);

#[tauri::command]
pub(super) async fn capture_checkpoint(app: AppHandle, stage: String) -> Result<(), String> {
    let (tx, rx) = mpsc::channel();
    app.state::<Checkpoints>()
        .0
        .send((stage, tx))
        .map_err(|e| e.to_string())?;
    tauri::async_runtime::spawn_blocking(move || rx.recv_timeout(Duration::from_secs(15)))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
}

pub(super) fn exercise(app: &AppHandle) {
    assert!(
        !crate::portable::is_portable(),
        "native IPC test must never use a portable user store"
    );
    assert!(app
        .config()
        .identifier
        .starts_with("com.handy.capture-test-"));
    let store = app
        .store_builder(settings::SETTINGS_STORE_PATH)
        .disable_auto_save()
        .build()
        .unwrap();
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
    settings::write_settings(app, current);
    // Drain the previous native harness's lifecycle requests before entering
    // the independent public-command scenario (no user-command retry here).
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let permit = loop {
        match admit(app) {
            Ok(permit) => break permit,
            Err(error) if error.contains("busy") && std::time::Instant::now() < deadline => {
                std::thread::yield_now()
            }
            Err(error) => panic!("native capture setup: {error}"),
        }
    };
    crate::shortcut::init_shortcuts(app).unwrap();
    app.manage(crate::commands::ShortcutsInitialized);
    drop(permit);
    let (capture_tx, capture_rx) = mpsc::channel();
    let listener = app.listen("handy-keys-event", move |event| {
        capture_tx.send(event.payload().to_string()).unwrap();
    });
    let (tx, rx) = mpsc::channel();
    app.manage(Checkpoints(tx));
    let window = tauri::WebviewWindowBuilder::new(
        app,
        "capture-ipc-test",
        tauri::WebviewUrl::CustomProtocol("capture-test://localhost/".parse().unwrap()),
    )
    .visible(false)
    .build()
    .unwrap();

    for expected in ["recording", "stopped", "tauri-editor", "restarted", "finished"] {
        let (stage, proceed) = rx
            .recv_timeout(Duration::from_secs(30))
            .expect("capture IPC checkpoint timed out");
        assert_eq!(stage, expected, "production IPC sequence failed");
        if expected == "tauri-editor" {
            assert!(intent(app).unwrap().1, "stale HandyKeys stop resumed the Tauri editor's suspension");
        }
        if matches!(expected, "recording" | "restarted") {
            assert!(handy_keys::owns_capture_suspension(app).unwrap());
            let mut keyboard = Enigo::new(&enigo::Settings::default()).unwrap();
            keyboard.key(Key::F21, Direction::Press).unwrap();
            let pressed = capture_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("no native capture event");
            keyboard.key(Key::F21, Direction::Release).unwrap();
            let released = capture_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("no native release event");
            assert!(
                pressed.contains("f21") || pressed.contains("F21"),
                "{pressed}"
            );
            assert!(
                released.contains("f21") || released.contains("F21"),
                "{released}"
            );
        } else {
            handy_keys::assert_capture_stopped(app);
            assert!(
                capture_rx.try_recv().is_err(),
                "old capture emitted after stop"
            );
            let mut keyboard = Enigo::new(&enigo::Settings::default()).unwrap();
            keyboard.key(Key::F21, Direction::Click).unwrap();
            // Listener and worker are both gone, not merely a quiet timeout.
            handy_keys::assert_capture_stopped(app);
            assert!(
                capture_rx.try_recv().is_err(),
                "terminated session still emitted"
            );
        }
        proceed.send(()).unwrap();
    }
    window.close().unwrap();
    app.unlisten(listener);
    for entry in snapshot(app).unwrap() {
        Native(app).remove(&entry).unwrap();
    }
    // No test settings are saved on the plugin's Exit callback.
    store.close_resource();
    println!("Windows real WebView IPC: start -> switch to Tauri -> repeated stop -> switch back -> start/stop; native capture events delivered only by live sessions.");
}
