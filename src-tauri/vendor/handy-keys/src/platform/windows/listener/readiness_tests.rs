use super::*;
use std::time::Duration;

fn shutdown(mut state: WindowsListenerState) {
    // The public Listener normally owns these fields and performs this join.
    state.running.store(false, Ordering::SeqCst);
    state.thread_handle.take().unwrap().join().unwrap();
}

#[test]
fn constructor_waits_for_native_hook_setup() {
    let (entered, entry) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let (completed, completion) = mpsc::channel();
    let caller = thread::spawn(move || {
        let result = spawn_inner(None, move |stage| {
            if stage == HookStage::Keyboard {
                entered.send(()).unwrap();
                gate.recv().unwrap();
            }
            Ok(())
        });
        completed.send(result).unwrap();
    });
    entry.recv_timeout(Duration::from_secs(5)).unwrap();
    let premature = completion.recv_timeout(Duration::from_millis(100));
    // Always release/join, even when exercising the old early-return bug.
    release.send(()).unwrap();
    let returned_early = premature.is_ok();
    let result = match premature {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            completion.recv_timeout(Duration::from_secs(5)).unwrap()
        }
        Err(error) => panic!("constructor disconnected: {error}"),
    };
    shutdown(result.unwrap());
    caller.join().unwrap();
    assert!(
        !returned_early,
        "constructor returned before native hook setup"
    );
}

#[test]
fn keyboard_install_failure_reaches_constructor() {
    let result = spawn_inner(None, |stage| {
        assert!(stage == HookStage::Keyboard);
        Err(windows::core::Error::from_hresult(
            windows::Win32::Foundation::E_FAIL,
        ))
    });
    assert!(
        matches!(result, Err(crate::Error::Platform(ref message)) if message.contains("keyboard hook"))
    );
}

#[test]
fn mouse_install_failure_reaches_constructor_after_keyboard_cleanup() {
    let result = spawn_inner(None, |stage| {
        if stage == HookStage::Mouse {
            Err(windows::core::Error::from_hresult(
                windows::Win32::Foundation::E_FAIL,
            ))
        } else {
            Ok(())
        }
    });
    assert!(
        matches!(result, Err(crate::Error::Platform(ref message)) if message.contains("mouse hook"))
    );
    // A subsequent native setup and shutdown must still work.
    shutdown(spawn(None).unwrap());
}

#[test]
fn setup_thread_panic_is_not_success() {
    let result = spawn_inner(None, |_| panic!("injected pre-hook setup panic"));
    assert!(
        matches!(result, Err(crate::Error::Platform(ref message)) if message.contains("before reporting readiness"))
    );
}
