//! No-input ordering experiments, not permission to weaken native-focus gates.
//! The deterministic model pins callback ordering; the ignored owned native
//! fixture exercises actual UIA/Win32 primitives with a fake insertion counter.
//! Neither test proves the cause of a WebView2/Tauri incident.

use super::*;
use crate::uia::cache::{InputUiaOwnership, InputUiaScopeWitness, KeyboardUiaFocusWitness};
use std::cell::{Cell, RefCell};

fn renderer_ready() -> KeyboardNativeWitness {
    KeyboardNativeWitness {
        target: 10,
        target_thread: 2,
        target_pid: 3,
        root: 10,
        foreground: 10,
        foreground_thread: 2,
        active: 10,
        focus: 13,
        focus_thread: 22,
        focus_pid: 33,
    }
}

fn ready_uia() -> KeyboardUiaFocusWitness {
    KeyboardUiaFocusWitness {
        has_focus: true,
        focused_equals_target: true,
        process_matches: true,
        own_native_hwnd: 0,
        nearest_native_hwnd: 12,
        value_changed_on_focus: None,
        scope: Some(InputUiaScopeWitness {
            own_native_hwnd: 0,
            nearest_native_hwnd: 12,
            provider_pid: 77,
            anchor_pid: 33,
            anchor_thread: 22,
            anchor_parent: 13,
            anchor_root: 10,
            ownership: InputUiaOwnership::VirtualDocument,
        }),
    }
}

#[test]
fn semantic_focus_ready_does_not_replace_the_native_baseline() {
    let settled = renderer_ready();
    let mut initial = settled;
    initial.focus = initial.target;
    initial.focus_pid = initial.target_pid;
    initial.focus_thread = initial.target_thread;
    let observed_native = Cell::new(initial);
    let semantic_ready = Cell::new(true);
    let sends = Cell::new(0);
    let phases = RefCell::new(Vec::new());

    // A controlled asynchronous preparation completion occurs in the provider
    // interval. Semantic focus was already true before the native baseline.
    let failure = dispatch_retained_key_once(
        KeyboardFocusWitness::pending(KeyboardElementReferenceKind::Token),
        || {
            assert!(semantic_ready.get());
            phases.borrow_mut().push("before");
            Ok(observed_native.get())
        },
        || {
            phases.borrow_mut().push("uia");
            observed_native.set(settled);
            Ok(ready_uia())
        },
        |_| {
            phases.borrow_mut().push("final");
            Ok((
                observed_native.get(),
                KeyboardNativeCorrelation::SharedAncestor,
            ))
        },
        || {
            sends.set(sends.get() + 1);
            2
        },
        2,
    )
    .unwrap_err();
    assert_eq!(*phases.borrow(), ["before", "uia", "final"]);
    assert!(matches!(
        failure.witness.failure_code,
        Some(KeyboardWitnessFailureCode::NativeFocusChanged)
    ));
    assert_eq!(failure.witness.native_foreground_stable, Some(true));
    assert_eq!(failure.witness.native_focus_stable, Some(false));
    assert_eq!(failure.witness.uia_has_keyboard_focus, Some(true));
    assert_eq!(
        failure.witness.uia_focused_element_equals_target,
        Some(true)
    );
    assert_eq!(failure.witness.uia_process_matches_target, Some(true));
    assert!(!failure.witness.input_attempted);
    assert_eq!(failure.witness.inserted_event_count, 0);
    assert_eq!(sends.get(), 0);

    // A separate operation whose preparation finished BEFORE its unchanged
    // baseline passes. This is not a retry/re-baselining of the refused action.
    let prepared_sends = Cell::new(0);
    let success = dispatch_retained_key_once(
        KeyboardFocusWitness::pending(KeyboardElementReferenceKind::Token),
        || Ok(settled),
        || Ok(ready_uia()),
        |_| Ok((settled, KeyboardNativeCorrelation::SharedAncestor)),
        || {
            prepared_sends.set(prepared_sends.get() + 1);
            2
        },
        2,
    )
    .unwrap();
    assert_eq!(success.native_focus_stable, Some(true));
    assert!(success.input_attempted);
    assert_eq!(success.inserted_event_count, 2);
    assert_eq!(prepared_sends.get(), 1);
}

#[test]
#[ignore = "requires exclusive interactive Windows desktop; owned windows only; no SendInput"]
fn owned_native_pre_uia_to_final_focus_drift_refuses_without_input() {
    use crate::uia::cache::keyboard_focus_owned_fixture_tests::{
        retained_native, Fixture, TestCom,
    };
    let fixture = Fixture::start();
    let _com = TestCom::start();
    let retained = retained_native(fixture.first);
    let pid = std::process::id();
    let sends = Cell::new(0);
    fixture.focus(true);
    let first_before = fixture.observe(false);
    let sibling_before = fixture.observe(true);

    // The owned worker acknowledges one native focus completion only in the
    // UIA interval. Actual native samples bracket it; no numeric witness is
    // rewritten, and the real final correlation must succeed on the target.
    let failure = dispatch_retained_key_once(
        KeyboardFocusWitness::pending(KeyboardElementReferenceKind::Token),
        || {
            let before = read_keyboard_native_witness(fixture.top, pid)?;
            assert_eq!(before.focus, fixture.second);
            Ok(before)
        },
        || {
            fixture.focus(false);
            retained.keyboard_focus_witness(fixture.top, pid, None)
        },
        |uia| read_final_keyboard_witness(fixture.top, pid, uia),
        || {
            sends.set(sends.get() + 1);
            2
        },
        2,
    )
    .unwrap_err();
    assert!(matches!(
        failure.witness.failure_code,
        Some(KeyboardWitnessFailureCode::NativeFocusChanged)
    ));
    assert_eq!(failure.witness.native_foreground_stable, Some(true));
    assert_eq!(failure.witness.native_focus_stable, Some(false));
    assert_eq!(failure.witness.uia_has_keyboard_focus, Some(true));
    assert_eq!(
        failure.witness.uia_focused_element_equals_target,
        Some(true)
    );
    assert_eq!(failure.witness.uia_process_matches_target, Some(true));
    assert!(!failure.witness.input_attempted);
    assert_eq!(failure.witness.inserted_event_count, 0);
    assert_eq!(sends.get(), 0);

    // Stable preparation is a separate no-input experiment, not automatic
    // retry. Use the existing real-primitive seam and its fake sender only.
    let stable_sends = Cell::new(0);
    let stable =
        dispatch_retained_key_for_owned_fixture(fixture.top, pid, &retained, &stable_sends)
            .unwrap();
    assert_eq!(stable.native_focus_stable, Some(true));
    assert_eq!(stable_sends.get(), 1);
    assert!(fixture.observe(false) == first_before);
    assert!(fixture.observe(true) == sibling_before);
}
