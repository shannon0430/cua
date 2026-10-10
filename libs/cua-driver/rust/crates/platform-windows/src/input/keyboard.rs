//! Background keyboard injection via PostMessage.
//!
//! WM_CHAR — posts a character code; simpler and more reliable for text entry.
//! WM_KEYDOWN/WM_KEYUP — used for non-printable keys (Enter, Tab, arrows, F-keys, etc.)
//!
//! All messages are posted async (PostMessageW), so they do NOT steal focus.
//!
//! Modern XAML / WinUI3 / UWP targets reject PostMessage-based keyboard
//! injection because their CoreInput dispatcher only consumes events from
//! the system input queue, not posted messages. This module exposes
//! [`is_xaml_host_hwnd`] so callers (e.g. the `type_text` tool) can route
//! around the PostMessage path for those targets — see CUA-543 and
//! `tools::impl_::TypeTextTool` for the routing logic. The actual UIA
//! `ValuePattern.SetValue` injection lives in the tools layer alongside
//! the existing `set_value` tool; this module deliberately stays
//! Win32-only so the unit tests don't depend on UIA initialisation.

use anyhow::{bail, Result};
use std::thread::sleep;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{BOOL, HWND, LPARAM, TRUE, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    MapVirtualKeyW, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, KEYEVENTF_UNICODE, MAPVK_VK_TO_VSC,
    VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumChildWindows, GetClassNameW, GetGUIThreadInfo, GetParent, GetWindowThreadProcessId,
    IsChild, PostMessageW, GUITHREADINFO, WM_CHAR, WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN,
    WM_SYSKEYUP,
};
use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, SetForegroundWindow};

// ── XAML / UWP host detection ────────────────────────────────────────────────
//
// Two routing signals, OR'd:
//   1. Top-level window class name matches a known XAML host class.
//   2. Owning process .exe basename matches a known XAML-hosted .exe.
//
// The EXE-basename signal is the more reliable of the two: cross-session
// `GetClassNameW` can return nothing, and modern apps like Win 11 Notepad
// keep the legacy `"Notepad"` window class even though they render XAML
// underneath. Diagnostic data captured by `tools::DebugWindowInfoTool`
// (see CUA-543) confirms `notepad.exe` is the reliable signal for modern
// Notepad; class name is not.

const XAML_HOST_CLASSES: &[&str] = &[
    "ApplicationFrameWindow",
    "WinUIDesktopWin32WindowClass",
    "Windows.UI.Core.CoreWindow",
    "Microsoft.UI.Content.DesktopChildSiteBridge",
];

const XAML_HOST_EXES: &[&str] = &[
    "notepad.exe",              // Win 11 modern Notepad (UWP-packaged)
    "calculatorapp.exe",        // UWP Calculator
    "calc.exe",                 // some Win 11 builds expose the stub directly
    "applicationframehost.exe", // generic UWP frame host
    "photos.exe",               // UWP Photos
    "systemsettings.exe",       // modern Settings
];

fn class_name(hwnd: HWND) -> Option<String> {
    let mut buf = [0u16; 256];
    let n = unsafe { GetClassNameW(hwnd, &mut buf) };
    if n <= 0 {
        None
    } else {
        Some(String::from_utf16_lossy(&buf[..n as usize]))
    }
}

fn owning_exe_basename(hwnd: HWND) -> Option<String> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_FORMAT,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };

    let mut pid: u32 = 0;
    let tid = unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    if tid == 0 || pid == 0 {
        return None;
    }
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
    let mut buf = [0u16; 1024];
    let mut len: u32 = buf.len() as u32;
    let result = unsafe {
        QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_FORMAT(0),
            windows::core::PWSTR(buf.as_mut_ptr()),
            &mut len,
        )
    };
    let _ = unsafe { CloseHandle(handle) };
    if result.is_err() || len == 0 {
        return None;
    }
    let path = String::from_utf16_lossy(&buf[..len as usize]);
    let name = path
        .rsplit(|c: char| c == '\\' || c == '/')
        .next()
        .unwrap_or(&path)
        .to_ascii_lowercase();
    Some(name)
}

/// `true` iff the given HWND should bypass the PostMessage keyboard
/// path and route through UIA patterns (or another non-PostMessage
/// mechanism). See module docs + CUA-543 for the routing rationale.
pub fn is_xaml_host_hwnd(hwnd: u64) -> bool {
    let h = HWND(hwnd as *mut _);
    if let Some(cls) = class_name(h) {
        if XAML_HOST_CLASSES.iter().any(|known| cls == *known) {
            return true;
        }
    }
    if let Some(exe) = owning_exe_basename(h) {
        if XAML_HOST_EXES.iter().any(|known| exe == *known) {
            return true;
        }
    }
    false
}

const KEY_DELAY_MS: u64 = 4;

/// If any UI thread under the target has a focused child window that's a
/// descendant of `parent`, return that child. Otherwise `None`. Used to retarget
/// `PostMessage(WM_CHAR/WM_KEYDOWN)` from the top-level frame to the actual
/// editor control (Scintilla in Notepad++, RichEdit in WordPad, etc.) —
/// top-level WindowProcs don't forward keyboard messages to embedded editors
/// automatically, so without this drill-down `type_text` silently no-ops
/// against any app that puts its text surface in a child HWND.
///
/// Embedded renderers such as WebView2 may put their focused child on a
/// different UI thread from the native top-level frame. Enumerating descendant
/// thread ids is therefore required; checking only the frame thread queues the
/// message successfully but leaves the renderer untouched. More than one of
/// those threads can retain a focused HWND, so choose the deepest focused
/// descendant rather than whichever thread happens to enumerate first.
fn focused_descendant(parent: HWND) -> Option<HWND> {
    if parent.0.is_null() {
        return None;
    }
    let parent_thread = unsafe { GetWindowThreadProcessId(parent, None) };
    if parent_thread == 0 {
        return None;
    }

    unsafe extern "system" fn collect_thread(child: HWND, lparam: LPARAM) -> BOOL {
        let threads = &mut *(lparam.0 as *mut Vec<u32>);
        let thread = GetWindowThreadProcessId(child, None);
        if thread != 0 && !threads.contains(&thread) {
            threads.push(thread);
        }
        TRUE
    }

    let mut target_threads = vec![parent_thread];
    unsafe {
        let _ = EnumChildWindows(
            parent,
            Some(collect_thread),
            LPARAM(&mut target_threads as *mut Vec<u32> as isize),
        );
    }
    let mut best: Option<(usize, HWND)> = None;
    for target_thread in target_threads {
        let mut info = GUITHREADINFO {
            cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
            ..Default::default()
        };
        if unsafe { GetGUIThreadInfo(target_thread, &mut info) }.is_err() {
            continue;
        }
        let focused = info.hwndFocus;
        if focused.0.is_null()
            || focused == parent
            || !unsafe { IsChild(parent, focused) }.as_bool()
        {
            continue;
        }

        let mut depth = 0usize;
        let mut current = focused;
        while current != parent && depth < 64 {
            let Ok(next) = (unsafe { GetParent(current) }) else {
                break;
            };
            if next.0.is_null() {
                break;
            }
            depth += 1;
            current = next;
        }
        if current == parent && best.as_ref().map_or(true, |(d, _)| depth > *d) {
            best = Some((depth, focused));
        }
    }
    best.map(|(_, focused)| focused)
}

/// Wait for an element-focused embedded renderer to expose its child HWND.
/// UIA SetFocus can complete before WebView2 updates GUITHREADINFO; polling the
/// observable focus target avoids posting the key to the native frame in that
/// short interval.
pub fn wait_for_focused_descendant(hwnd: u64, timeout: Duration) -> Option<u64> {
    let parent = HWND(hwnd as *mut _);
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(target) = focused_descendant(parent) {
            return Some(target.0 as usize as u64);
        }
        if Instant::now() >= deadline {
            return None;
        }
        sleep(Duration::from_millis(10));
    }
}

/// Post a Unicode character as WM_CHAR.
pub fn post_char(hwnd: u64, ch: char) -> Result<()> {
    if let Some(msg) = crate::input::post_message_blocked_by_uipi(hwnd) {
        anyhow::bail!(msg);
    }
    // Retarget to the focused child if any — top-level WindowProcs typically
    // don't forward WM_CHAR to embedded editor children (Scintilla, RichEdit).
    let h_parent = HWND(hwnd as *mut _);
    let h = focused_descendant(h_parent).unwrap_or(h_parent);
    let code = ch as u32 as usize;
    unsafe {
        PostMessageW(h, WM_CHAR, WPARAM(code), LPARAM(1))?;
    }
    Ok(())
}

/// Post `\n` (or `\r`, or `\r\n`) as a real Enter keystroke pair —
/// WM_KEYDOWN/WM_KEYUP(VK_RETURN) — to the focused child `h`.
///
/// Why a keystroke and not WM_CHAR(0x0A) or WM_CHAR(0x0D): standard Win32
/// edit controls accept `WM_CHAR(0x0D)` (carriage return) and insert a
/// newline; richer controls (LibreOffice's VCL edit, modern WPF/WinForms,
/// Scintilla) ignore `0x0A` outright and only sometimes accept `0x0D`.
/// A real `WM_KEYDOWN(VK_RETURN)` is the universally-honoured "Enter
/// pressed" signal — it produces a paragraph break in word processors,
/// activates the default button in dialogs, and submits forms in browsers.
/// The previous `WM_CHAR(0x0A)`-as-newline path silently joined every
/// line of multi-line `type_text` input into a single run (visible in the
/// LibreOffice Writer ode-to-a-background-cursor screenshot).
unsafe fn post_enter_keystroke(h: HWND) -> Result<()> {
    let vk = windows::Win32::UI::Input::KeyboardAndMouse::VK_RETURN;
    let scan = MapVirtualKeyW(vk.0 as u32, MAPVK_VK_TO_VSC);
    let lp_down = 1u32 | (scan << 16);
    let lp_up = lp_down | (1u32 << 30) | (1u32 << 31);
    PostMessageW(
        h,
        WM_KEYDOWN,
        WPARAM(vk.0 as usize),
        LPARAM(lp_down as isize),
    )?;
    // Hold time between KEYDOWN and KEYUP — matches `post_key`'s pattern and
    // gives the target's message loop time to TranslateMessage the KEYDOWN
    // (which synthesizes WM_CHAR(0x0D)) and DispatchMessage the paragraph
    // break before the KEYUP arrives. Without this gap, LibreOffice Writer
    // intermittently produced a double paragraph break AND dropped the
    // next character — visible in the "ABC\nDEF\nGHI" repro as "ABC / DEF /
    // (gap) / HI".
    sleep(Duration::from_millis(KEY_DELAY_MS));
    PostMessageW(h, WM_KEYUP, WPARAM(vk.0 as usize), LPARAM(lp_up as isize))?;
    Ok(())
}

/// Post all characters in a string as WM_CHAR messages with inter-key delay.
///
/// Line breaks (`\n`, `\r`, or `\r\n`) are emitted as Enter keystrokes
/// (see [`post_enter_keystroke`]) instead of literal `WM_CHAR(0x0A/0x0D)`,
/// which most rich-text Win32 controls drop.
pub fn post_type_text(hwnd: u64, text: &str) -> Result<()> {
    post_type_text_with_delay(hwnd, text, 0)
}

/// Post all characters in `text` as WM_CHAR messages with a configurable
/// inter-character delay (on top of the baseline KEY_DELAY_MS gap).
///
/// Line breaks (`\n`, `\r`, or `\r\n`) are emitted as Enter keystrokes
/// (see [`post_enter_keystroke`]) — see the LibreOffice screenshot in
/// the PR description for the bug this fixes.
pub fn post_type_text_with_delay(hwnd: u64, text: &str, inter_char_ms: u64) -> Result<()> {
    if let Some(msg) = crate::input::post_message_blocked_by_uipi(hwnd) {
        anyhow::bail!(msg);
    }
    let h_parent = HWND(hwnd as *mut _);
    // Resolve focused-child once at entry — re-querying per-character would
    // race with text-insertion side effects on the focus.
    let h = focused_descendant(h_parent).unwrap_or(h_parent);

    let mut prev_was_cr = false;
    for ch in text.chars() {
        match ch {
            // `\r\n` (Windows-style line ending) → single Enter. The `\r`
            // does the Enter; the following `\n` is consumed silently.
            '\n' if prev_was_cr => {
                prev_was_cr = false;
            }
            '\n' | '\r' => {
                unsafe {
                    post_enter_keystroke(h)?;
                }
                prev_was_cr = ch == '\r';
                // Extra settle after Enter — paragraph creation in rich
                // editors (VCL Writer, Scintilla, RichEdit) is heavier than
                // a single-character insert, so the baseline KEY_DELAY_MS +
                // inter_char_ms is not enough on slower hosts. The extra
                // 20 ms covers the queue drain without noticeably slowing
                // multi-line typing.
                sleep(Duration::from_millis(KEY_DELAY_MS + inter_char_ms + 20));
            }
            _ => {
                prev_was_cr = false;
                let code = ch as u32 as usize;
                unsafe {
                    PostMessageW(h, WM_CHAR, WPARAM(code), LPARAM(1))?;
                }
                sleep(Duration::from_millis(KEY_DELAY_MS + inter_char_ms));
            }
        }
    }
    Ok(())
}

/// Press a named key (and optional modifiers) via WM_KEYDOWN/WM_KEYUP.
pub fn post_key(hwnd: u64, key: &str, modifiers: &[&str]) -> Result<()> {
    if let Some(msg) = crate::input::post_message_blocked_by_uipi(hwnd) {
        anyhow::bail!(msg);
    }
    let hwnd_win = HWND(hwnd as *mut _);
    // WebView2/Tauri keeps the editable renderer in a focused child HWND.
    // Posting only to the top-level frame reports success but never reaches
    // the renderer; mirror the WM_CHAR path and retarget to that child when
    // the target thread exposes one.
    let target = focused_descendant(hwnd_win).unwrap_or(hwnd_win);
    let vk = key_name_to_vk(key)?;
    let has_alt = modifiers.iter().any(|m| *m == "alt" || *m == "menu");

    let scan = unsafe { MapVirtualKeyW(vk.0 as u32, MAPVK_VK_TO_VSC) };
    let repeat_lp = |scan: u32, extended: bool, key_up: bool| {
        let mut lp: u32 = 1; // repeat count
        lp |= scan << 16;
        if extended {
            lp |= 1 << 24;
        }
        if key_up {
            lp |= (1 << 30) | (1 << 31);
        }
        LPARAM(lp as isize)
    };

    let (down_msg, up_msg) = if has_alt {
        (WM_SYSKEYDOWN, WM_SYSKEYUP)
    } else {
        (WM_KEYDOWN, WM_KEYUP)
    };

    let mod_vks: Vec<VIRTUAL_KEY> = modifiers.iter().filter_map(|m| modifier_vk(m)).collect();

    unsafe {
        // Press modifiers.
        for mvk in &mod_vks {
            let ms = MapVirtualKeyW(mvk.0 as u32, MAPVK_VK_TO_VSC);
            PostMessageW(
                target,
                down_msg,
                WPARAM(mvk.0 as usize),
                repeat_lp(ms, false, false),
            )?;
        }
        // Press key.
        PostMessageW(
            target,
            down_msg,
            WPARAM(vk.0 as usize),
            repeat_lp(scan, is_extended(vk), false),
        )?;
        sleep(Duration::from_millis(KEY_DELAY_MS));
        // Release key.
        PostMessageW(
            target,
            up_msg,
            WPARAM(vk.0 as usize),
            repeat_lp(scan, is_extended(vk), true),
        )?;
        // Release modifiers (reverse order).
        for mvk in mod_vks.iter().rev() {
            let ms = MapVirtualKeyW(mvk.0 as u32, MAPVK_VK_TO_VSC);
            PostMessageW(
                target,
                up_msg,
                WPARAM(mvk.0 as usize),
                repeat_lp(ms, false, true),
            )?;
        }
    }
    Ok(())
}

/// Press `key` (with optional `modifiers`) via `SendInput` against the system
/// input queue, briefly focusing `hwnd` so the keystrokes land there.
///
/// Why this exists alongside `post_key`: `PostMessage(WM_KEYDOWN, VK_CONTROL)`
/// puts a message in the target's queue but does NOT update the system-wide
/// modifier state that apps poll via `GetKeyState` / `GetAsyncKeyState`. For
/// any Win32 app whose accelerator dispatcher uses `TranslateAccelerator` (which
/// is most native Win32 apps — LibreOffice, FAR, classic Notepad, etc.), the
/// shortcut never fires; the `s` arrives as plain text input.
///
/// `SendInput` puts the synthesized events on the **system input queue** —
/// the same queue `GetKeyState` reads from — so `Ctrl+S` is properly detected
/// as an accelerator. The trade-off is a brief foreground swap (focus theft),
/// which we mitigate by saving the previous foreground HWND and restoring it
/// after the keystrokes are flushed.
///
/// UIAccess constraint: `SetForegroundWindow` is restricted from non-UIAccess
/// processes when not driven by user input. The `cua-driver-uia` worker runs
/// at UIAccess integrity precisely so this restriction is lifted; outside the
/// worker, the foreground swap may silently fail and SendInput land on the
/// wrong window. Callers should funnel hotkey calls through the uia worker.
pub fn send_key_synthesized(hwnd: u64, key: &str, modifiers: &[&str]) -> Result<()> {
    send_key_synthesized_after_focus(hwnd, key, modifiers, || Ok(()))
}

/// Foreground key delivery with a target-specific focus step performed only
/// after Windows has confirmed the exact top-level HWND as foreground.
///
/// Element-addressed callers use this to avoid the activation/focus race:
/// UIA `SetFocus` before `SetForegroundWindow` can be overwritten by the
/// ensuing activation. The callback may establish and verify child focus; no
/// input is inserted when either foreground or child-focus confirmation fails.
pub fn send_key_synthesized_after_focus(
    hwnd: u64,
    key: &str,
    modifiers: &[&str],
    focus: impl FnOnce() -> Result<()>,
) -> Result<()> {
    let target = HWND(hwnd as *mut _);
    if target.0.is_null() {
        bail!("invalid target hwnd");
    }
    if let Some(msg) = crate::input::post_message_blocked_by_uipi(hwnd) {
        // Same UIPI defense as the PostMessage path. SendInput from UIAccess
        // _is_ allowed cross-integrity, but if our daemon is somehow at a
        // lower integrity than target, SendInput would land in the wrong
        // window (we couldn't set foreground). Better to surface the
        // diagnostic early than silently no-op.
        bail!(msg);
    }
    let key_vk = key_name_to_vk(key)?;
    let mod_vks: Vec<VIRTUAL_KEY> = modifiers.iter().filter_map(|m| modifier_vk(m)).collect();

    // Build the INPUT sequence: modifiers down, key down, key up, modifiers up
    // (reverse order). Each event sends the scancode + EXTENDEDKEY flag where
    // appropriate so apps that read scancodes (not virtual keys) work too.
    let mut events: Vec<INPUT> = Vec::with_capacity(mod_vks.len() * 2 + 2);
    for mvk in &mod_vks {
        events.push(key_input(*mvk, false));
    }
    events.push(key_input(key_vk, false));
    events.push(key_input(key_vk, true));
    for mvk in mod_vks.iter().rev() {
        events.push(key_input(*mvk, true));
    }

    with_confirmed_foreground(target, "key delivery", focus, || unsafe {
        let sent = SendInput(&events, std::mem::size_of::<INPUT>() as i32);
        if sent as usize != events.len() {
            bail!(
                "SendInput inserted only {sent} of {} events. Likely cause: \
                 the daemon is not at UIAccess integrity, so SetForegroundWindow \
                 was rejected and the events landed on the wrong window. Run \
                 hotkey through the cua-driver-uia worker.",
                events.len()
            );
        }
        Ok(())
    })
}

/// Wire payload intentionally contains only closed classifications, booleans
/// and counts. It is not an application effect or keyboard-consumption proof.
pub(crate) const KEYBOARD_FOCUS_WITNESS_MARKER: &str = "CUA_KEYBOARD_FOCUS_WITNESS_V1 ";
#[derive(Clone, Copy, Debug, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum KeyboardElementReferenceKind {
    Token,
    IndexSnapshot,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum KeyboardNativeCorrelation {
    ExactNativeChild,
    SharedAncestor,
    Unavailable,
}
#[derive(Clone, Copy, Debug, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum KeyboardForegroundRelation {
    Exact,
    Other,
    Unavailable,
}
#[derive(Clone, Copy, Debug, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum KeyboardWitnessFailureCode {
    TargetUnavailable,
    TargetUnsupported,
    PreparationFailed,
    UiaUnavailable,
    UiaFocusMismatch,
    UiaProcessMismatch,
    UiaBoundaryMismatch,
    NativeFocusUnavailable,
    NativeFocusMismatch,
    NativeFocusChanged,
    InputPartial,
}
#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct KeyboardFocusWitness {
    pub version: u8,
    pub native_element_admitted: bool,
    pub element_reference_kind: KeyboardElementReferenceKind,
    pub uia_has_keyboard_focus: Option<bool>,
    pub uia_focused_element_equals_target: Option<bool>,
    /// True only for proved target-boundary process ownership: direct PID
    /// equality, or the exact windowless Document/native-child proof path.
    /// It does not assert leaf PID numerically equals frame PID on virtual input.
    pub uia_process_matches_target: Option<bool>,
    pub native_foreground_relation: KeyboardForegroundRelation,
    pub native_foreground_stable: Option<bool>,
    pub native_gui_info_available: bool,
    pub native_active_within_boundary: Option<bool>,
    pub native_focus_within_boundary: Option<bool>,
    pub native_focus_stable: Option<bool>,
    pub native_element_correlation: KeyboardNativeCorrelation,
    pub input_attempted: bool,
    pub inserted_event_count: u32,
    pub value_changed_on_focus: Option<bool>,
    pub provider_failure_code: Option<i32>,
    pub failure_code: Option<KeyboardWitnessFailureCode>,
}
impl KeyboardFocusWitness {
    fn pending(kind: KeyboardElementReferenceKind) -> Self {
        Self {
            version: 1,
            native_element_admitted: true,
            element_reference_kind: kind,
            uia_has_keyboard_focus: None,
            uia_focused_element_equals_target: None,
            uia_process_matches_target: None,
            native_foreground_relation: KeyboardForegroundRelation::Unavailable,
            native_foreground_stable: None,
            native_gui_info_available: false,
            native_active_within_boundary: None,
            native_focus_within_boundary: None,
            native_focus_stable: None,
            native_element_correlation: KeyboardNativeCorrelation::Unavailable,
            input_attempted: false,
            inserted_event_count: 0,
            value_changed_on_focus: None,
            provider_failure_code: None,
            failure_code: None,
        }
    }
    pub(crate) fn diagnostic_text(&self) -> String {
        // The fixed fields cannot contain arbitrary provider/request text.
        format!(
            "{}{}",
            KEYBOARD_FOCUS_WITNESS_MARKER,
            serde_json::to_string(self).expect("primitive witness serialization")
        )
    }
    fn fail(
        mut self,
        code: KeyboardWitnessFailureCode,
        hresult: Option<i32>,
    ) -> KeyboardWitnessFailure {
        self.failure_code = Some(code);
        self.provider_failure_code = hresult;
        KeyboardWitnessFailure {
            witness: self,
            native_rejection: None,
        }
    }
}
/// Private, content-free native subreason. It stays in the refusal prefix, not
/// the closed 18-field payload; no target IDs, titles, values or PID are logged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum KeyboardNativeRejection {
    Unclassified,
    AdmittedTarget,
    Foreground,
    ForegroundThread,
    GuiThreadQuery,
    ActiveBoundary,
    FocusBoundary,
    FocusThread,
    ForegroundChanged,
    AnchorUnavailable,
    AnchorIdentity,
    AnchorFocusRelation,
    NativeSnapshotChanged,
}
impl KeyboardNativeRejection {
    fn as_str(self) -> &'static str {
        match self {
            Self::Unclassified => "unclassified",
            Self::AdmittedTarget => "admitted_target",
            Self::Foreground => "foreground",
            Self::ForegroundThread => "foreground_thread",
            Self::GuiThreadQuery => "gui_thread_query",
            Self::ActiveBoundary => "active_boundary",
            Self::FocusBoundary => "focus_boundary",
            Self::FocusThread => "focus_thread",
            Self::ForegroundChanged => "foreground_changed",
            Self::AnchorUnavailable => "anchor_unavailable",
            Self::AnchorIdentity => "anchor_identity",
            Self::AnchorFocusRelation => "anchor_focus_relation",
            Self::NativeSnapshotChanged => "native_snapshot_changed",
        }
    }
}
#[derive(Clone, Copy, Debug)]
struct KeyboardNativeReadFailure {
    code: KeyboardWitnessFailureCode,
    reason: KeyboardNativeRejection,
}
impl From<KeyboardWitnessFailureCode> for KeyboardNativeReadFailure {
    fn from(code: KeyboardWitnessFailureCode) -> Self {
        Self {
            code,
            reason: KeyboardNativeRejection::Unclassified,
        }
    }
}
impl KeyboardWitnessFailureCode {
    fn at(self, reason: KeyboardNativeRejection) -> KeyboardNativeReadFailure {
        KeyboardNativeReadFailure { code: self, reason }
    }
}
impl KeyboardFocusWitness {
    fn fail_native(self, error: KeyboardNativeReadFailure) -> KeyboardWitnessFailure {
        let mut failure = self.fail(error.code, None);
        failure.native_rejection = Some(error.reason);
        failure
    }
}

#[derive(Debug)]
pub(crate) struct KeyboardWitnessFailure {
    pub witness: KeyboardFocusWitness,
    native_rejection: Option<KeyboardNativeRejection>,
}
impl std::fmt::Display for KeyboardWitnessFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // No HWND/PID, key text, provider text or native error message is exposed.
        write!(
            f,
            "keyboard_focus_unavailable: retained-element key delivery did not complete;"
        )?;
        if let Some(reason) = self.native_rejection {
            write!(f, " native_subreason={};", reason.as_str())?;
        }
        write!(f, "\n{}", self.witness.diagnostic_text())
    }
}
impl std::error::Error for KeyboardWitnessFailure {}

/// Private primitive identities, never serialized or retained in wire detail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct KeyboardNativeWitness {
    target: u64,
    target_thread: u32,
    target_pid: u32,
    root: u64,
    foreground: u64,
    foreground_thread: u32,
    active: u64,
    focus: u64,
    focus_thread: u32,
    focus_pid: u32,
}
fn read_keyboard_native_witness(
    hwnd: u64,
    expected_pid: u32,
) -> Result<KeyboardNativeWitness, KeyboardNativeReadFailure> {
    use windows::Win32::UI::WindowsAndMessaging::{GetAncestor, IsWindow, GA_ROOT};
    use KeyboardWitnessFailureCode as Failure;
    let target = HWND(hwnd as *mut _);
    if !unsafe { IsWindow(target) }.as_bool() {
        return Err(Failure::TargetUnavailable.at(KeyboardNativeRejection::AdmittedTarget));
    }
    let mut target_pid = 0;
    let target_thread = unsafe { GetWindowThreadProcessId(target, Some(&mut target_pid)) };
    let root = unsafe { GetAncestor(target, GA_ROOT) };
    if target_thread == 0 || target_pid != expected_pid || root != target {
        return Err(Failure::TargetUnavailable.at(KeyboardNativeRejection::AdmittedTarget));
    }
    let foreground = unsafe { GetForegroundWindow() };
    if foreground != target {
        return Err(Failure::NativeFocusMismatch.at(KeyboardNativeRejection::Foreground));
    }
    let mut foreground_pid = 0;
    let foreground_thread =
        unsafe { GetWindowThreadProcessId(foreground, Some(&mut foreground_pid)) };
    if foreground_thread == 0 || foreground_pid != expected_pid {
        return Err(Failure::NativeFocusMismatch.at(KeyboardNativeRejection::ForegroundThread));
    }
    // Query the actual foreground thread, not a target child thread that may
    // retain inactive focus. Check the resulting primitive identity again.
    let mut info = GUITHREADINFO {
        cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
        ..Default::default()
    };
    unsafe { GetGUIThreadInfo(foreground_thread, &mut info) }
        .map_err(|_| Failure::NativeFocusUnavailable.at(KeyboardNativeRejection::GuiThreadQuery))?;
    let within = |window: HWND| {
        !window.0.is_null()
            && unsafe { IsWindow(window) }.as_bool()
            && (window == target || unsafe { IsChild(target, window) }.as_bool())
            && unsafe { GetAncestor(window, GA_ROOT) } == target
    };
    if !within(info.hwndActive) {
        return Err(Failure::NativeFocusMismatch.at(KeyboardNativeRejection::ActiveBoundary));
    }
    if !within(info.hwndFocus) {
        return Err(Failure::NativeFocusMismatch.at(KeyboardNativeRejection::FocusBoundary));
    }
    let mut focus_pid = 0;
    let focus_thread = unsafe { GetWindowThreadProcessId(info.hwndFocus, Some(&mut focus_pid)) };
    if focus_thread == 0 || focus_pid == 0 {
        return Err(Failure::NativeFocusMismatch.at(KeyboardNativeRejection::FocusThread));
    }
    if unsafe { GetForegroundWindow() } != foreground {
        return Err(Failure::NativeFocusMismatch.at(KeyboardNativeRejection::ForegroundChanged));
    }
    // Recheck the admitted HWND after all GUI reads, without provider work.
    let mut final_pid = 0;
    if unsafe { GetWindowThreadProcessId(target, Some(&mut final_pid)) } != target_thread
        || final_pid != expected_pid
        || unsafe { GetAncestor(target, GA_ROOT) } != root
    {
        return Err(Failure::TargetUnavailable.at(KeyboardNativeRejection::AdmittedTarget));
    }
    Ok(KeyboardNativeWitness {
        target: hwnd,
        target_thread,
        target_pid,
        root: root.0 as usize as u64,
        foreground: foreground.0 as usize as u64,
        foreground_thread,
        active: info.hwndActive.0 as usize as u64,
        focus: info.hwndFocus.0 as usize as u64,
        focus_thread,
        focus_pid,
    })
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct KeyboardNativeScopeWitness {
    hwnd: u64,
    parent: u64,
    root: u64,
    thread: u32,
    pid: u32,
}

fn keyboard_native_scope_matches(
    native: KeyboardNativeWitness,
    uia: crate::uia::cache::KeyboardUiaFocusWitness,
    expected: crate::uia::cache::InputUiaScopeWitness,
    refreshed: crate::uia::cache::InputUiaScopeWitness,
    actual: KeyboardNativeScopeWitness,
) -> bool {
    use crate::uia::cache::InputUiaOwnership;
    let direct = expected.ownership == InputUiaOwnership::DirectProcess;
    native.target != 0
        && native.target == native.root
        && native.foreground == native.target
        && native.target_pid != 0
        && actual.thread != 0
        && actual.root == native.root
        && expected == refreshed
        && expected.nearest_native_hwnd == uia.nearest_native_hwnd
        && expected.own_native_hwnd == uia.own_native_hwnd
        && actual.hwnd == expected.nearest_native_hwnd
        && actual.pid == expected.anchor_pid
        && actual.thread == expected.anchor_thread
        && actual.parent == expected.anchor_parent
        && if direct {
            actual.pid == native.target_pid && native.focus_pid == native.target_pid
        } else {
            uia.own_native_hwnd == 0 && uia.nearest_native_hwnd != native.target
        }
}

/// Physical descendant correlation is unchanged. A fully proved virtual
/// Document may instead correlate to its exact immediate native host parent.
/// Semantic focus was refreshed before COM teardown; this is not consumption.
fn keyboard_native_focus_relation_admitted(
    native: KeyboardNativeWitness,
    uia: crate::uia::cache::KeyboardUiaFocusWitness,
    actual: KeyboardNativeScopeWitness,
    focus_within_anchor: bool,
) -> bool {
    use crate::uia::cache::InputUiaOwnership;
    if focus_within_anchor {
        return true;
    }
    let Some(scope) = uia.scope else {
        return false;
    };
    uia.has_focus
        && uia.focused_equals_target
        && uia.process_matches
        && scope.ownership == InputUiaOwnership::VirtualDocument
        && scope.provider_pid > 0
        && scope.own_native_hwnd == 0
        && uia.own_native_hwnd == 0
        && scope.nearest_native_hwnd == actual.hwnd
        && uia.nearest_native_hwnd == actual.hwnd
        && actual.hwnd != 0
        && actual.hwnd != native.root
        && scope.anchor_root == native.root
        && actual.root == native.root
        && actual.parent != 0
        && scope.anchor_parent == actual.parent
        && native.focus == actual.parent
        && actual.pid != 0
        && actual.thread != 0
        && scope.anchor_pid == actual.pid
        && scope.anchor_thread == actual.thread
        && native.focus_pid == actual.pid
        && native.focus_thread == actual.thread
}

fn native_keyboard_correlation(
    native: KeyboardNativeWitness,
    uia: crate::uia::cache::KeyboardUiaFocusWitness,
) -> Result<(KeyboardNativeCorrelation, KeyboardNativeScopeWitness), KeyboardNativeReadFailure> {
    use windows::Win32::UI::WindowsAndMessaging::{GetAncestor, IsWindow, GA_ROOT};
    let nearest = HWND(uia.nearest_native_hwnd as *mut _);
    let focus = HWND(native.focus as *mut _);
    let mut pid = 0;
    let thread = unsafe { GetWindowThreadProcessId(nearest, Some(&mut pid)) };
    let root = unsafe { GetAncestor(nearest, GA_ROOT) }.0 as usize as u64;
    let parent = match unsafe { GetParent(nearest) } {
        Ok(parent) => parent.0 as usize as u64,
        // Top-level windows may legitimately have no parent. An unreadable
        // descendant parent cannot establish stable native scope membership.
        Err(_) if uia.nearest_native_hwnd == native.target => 0,
        Err(_) => {
            return Err(KeyboardWitnessFailureCode::NativeFocusUnavailable
                .at(KeyboardNativeRejection::AnchorUnavailable))
        }
    };
    let scope = uia.scope.ok_or_else(|| {
        KeyboardWitnessFailureCode::NativeFocusUnavailable
            .at(KeyboardNativeRejection::AnchorUnavailable)
    })?;
    let refreshed_scope = crate::uia::cache::read_input_native_scope(scope, native.target)
        .map_err(|_| {
            KeyboardWitnessFailureCode::NativeFocusMismatch
                .at(KeyboardNativeRejection::AnchorIdentity)
        })?;
    let actual = KeyboardNativeScopeWitness {
        hwnd: uia.nearest_native_hwnd,
        parent,
        root,
        thread,
        pid,
    };
    if nearest.0.is_null()
        || !unsafe { IsWindow(nearest) }.as_bool()
        || !keyboard_native_scope_matches(native, uia, scope, refreshed_scope, actual)
    {
        return Err(KeyboardWitnessFailureCode::NativeFocusMismatch
            .at(KeyboardNativeRejection::AnchorIdentity));
    }
    let focus_within_anchor = nearest == focus || unsafe { IsChild(nearest, focus) }.as_bool();
    if !keyboard_native_focus_relation_admitted(native, uia, actual, focus_within_anchor) {
        // Diagnostic-only observer runs after this refusal decision. It cannot
        // authorize input and adds nothing to the accepted proof -> SendInput path.
        #[cfg(feature = "native-input-proof-diagnostics")]
        super::proof_diagnostics::record_refusal(
            super::proof_diagnostics::Snapshot {
                target: native.target,
                root: native.root,
                target_pid: native.target_pid,
                target_thread: native.target_thread,
                root_thread: native.foreground_thread,
                foreground: native.foreground,
                active: Some(native.active),
                focus: native.focus,
                hover: None,
                focus_pid: Some(native.focus_pid),
                focus_thread: Some(native.focus_thread),
                point: None,
                scope: scope.into(),
            },
            super::proof_diagnostics::Predicates::Key {
                anchor_identity: true, // the preceding unchanged identity guard passed
                focus_within_anchor,
            },
        );
        return Err(KeyboardWitnessFailureCode::NativeFocusMismatch
            .at(KeyboardNativeRejection::AnchorFocusRelation));
    }
    let correlation = if uia.own_native_hwnd != 0
        && uia.own_native_hwnd == native.focus
        && native.focus != native.target
    {
        KeyboardNativeCorrelation::ExactNativeChild
    } else {
        // Shared SDM/virtual control ancestor is correlation, not consumption.
        KeyboardNativeCorrelation::SharedAncestor
    };
    Ok((correlation, actual))
}
fn read_final_keyboard_witness(
    hwnd: u64,
    expected_pid: u32,
    uia: crate::uia::cache::KeyboardUiaFocusWitness,
) -> Result<(KeyboardNativeWitness, KeyboardNativeCorrelation), KeyboardNativeReadFailure> {
    let initial = read_keyboard_native_witness(hwnd, expected_pid)?;
    let (correlation, initial_scope) = native_keyboard_correlation(initial, uia)?;
    let refresh = read_keyboard_native_witness(hwnd, expected_pid)?;
    let (_, refreshed_scope) = native_keyboard_correlation(refresh, uia)?;
    // Sample current foreground + active/focus + target/root, then bind exact
    // physical anchor membership to THAT sample as well. Intermediate ancestor
    // drift may leave the focus leaf/root and direct parent unchanged.
    let final_native = read_keyboard_native_witness(hwnd, expected_pid)?;
    let (final_correlation, final_scope) = native_keyboard_correlation(final_native, uia)?;
    if initial != refresh
        || refresh != final_native
        || initial_scope != refreshed_scope
        || refreshed_scope != final_scope
        || correlation != final_correlation
    {
        return Err(KeyboardWitnessFailureCode::NativeFocusChanged
            .at(KeyboardNativeRejection::NativeSnapshotChanged));
    }
    Ok((final_native, correlation))
}

/// Production dispatch seam used by the platform function and unit tests.
/// UIA callback returns only Copy facts, with COM teardown completed. The
/// final callback performs native-only reads/correlation, then ONE insertion.
fn dispatch_retained_key_once(
    mut witness: KeyboardFocusWitness,
    read_before: impl FnOnce() -> Result<KeyboardNativeWitness, KeyboardNativeReadFailure>,
    read_uia: impl FnOnce() -> Result<
        crate::uia::cache::KeyboardUiaFocusWitness,
        crate::uia::cache::KeyboardUiaWitnessError,
    >,
    read_final: impl FnOnce(
        crate::uia::cache::KeyboardUiaFocusWitness,
    ) -> Result<
        (KeyboardNativeWitness, KeyboardNativeCorrelation),
        KeyboardNativeReadFailure,
    >,
    insert: impl FnOnce() -> u32,
    expected_events: u32,
) -> Result<KeyboardFocusWitness, KeyboardWitnessFailure> {
    use KeyboardWitnessFailureCode as Failure;
    let before = read_before().map_err(|error| witness.clone().fail_native(error))?;
    witness.native_foreground_relation = KeyboardForegroundRelation::Exact;
    witness.native_gui_info_available = true;
    witness.native_active_within_boundary = Some(true);
    witness.native_focus_within_boundary = Some(true);
    let uia = read_uia().map_err(|error| {
        use crate::uia::cache::KeyboardUiaWitnessError as UiaFailure;
        let (code, hresult) = match error {
            UiaFailure::Unavailable(hr) => (Failure::UiaUnavailable, hr),
            UiaFailure::FocusMismatch => (Failure::UiaFocusMismatch, None),
            UiaFailure::ProcessMismatch => (Failure::UiaProcessMismatch, None),
            UiaFailure::BoundaryMismatch => (Failure::UiaBoundaryMismatch, None),
        };
        witness.clone().fail(code, hresult)
    })?;
    witness.uia_has_keyboard_focus = Some(uia.has_focus);
    witness.uia_focused_element_equals_target = Some(uia.focused_equals_target);
    witness.uia_process_matches_target = Some(uia.process_matches);
    witness.value_changed_on_focus = uia.value_changed_on_focus;
    // Also enforce the returned classifications in this production seam;
    // detached fake/classifier tests cannot silently skip the actual gate.
    if !uia.process_matches {
        return Err(witness.fail(Failure::UiaProcessMismatch, None));
    }
    if !uia.has_focus || !uia.focused_equals_target {
        return Err(witness.fail(Failure::UiaFocusMismatch, None));
    }
    let (final_native, correlation) =
        read_final(uia).map_err(|error| witness.clone().fail_native(error))?;
    witness.native_foreground_stable = Some(
        before.foreground == final_native.foreground
            && before.foreground_thread == final_native.foreground_thread,
    );
    witness.native_focus_stable = Some(before == final_native);
    witness.native_element_correlation = correlation;
    if before != final_native {
        return Err(witness.fail(Failure::NativeFocusChanged, None));
    }
    if matches!(correlation, KeyboardNativeCorrelation::Unavailable) {
        return Err(witness.fail(Failure::NativeFocusUnavailable, None));
    }
    // No UIA/COM calls, destructor, waits or other actuator between this gate
    // and the existing SendInput closure. External OS races remain possible.
    witness.input_attempted = true;
    witness.inserted_event_count = insert();
    if witness.inserted_event_count != expected_events {
        return Err(witness.fail(Failure::InputPartial, None));
    }
    Ok(witness)
}

/// Retained-element press_key only. Existing activation/focus and key INPUT
/// encoding are unchanged; no added actuator, auto retry or blind fallback.
pub(crate) fn send_retained_key_with_witness(
    hwnd: u64,
    expected_pid: u32,
    key: &str,
    modifiers: &[&str],
    reference_kind: KeyboardElementReferenceKind,
    retained: &crate::uia::cache::RetainedElement,
    focus: impl FnOnce() -> Result<()>,
) -> Result<KeyboardFocusWitness, KeyboardWitnessFailure> {
    let pending = KeyboardFocusWitness::pending(reference_kind);
    let target = HWND(hwnd as *mut _);
    let mut actual_pid = 0;
    if unsafe { GetWindowThreadProcessId(target, Some(&mut actual_pid)) } == 0
        || actual_pid != expected_pid
    {
        return Err(pending.fail(KeyboardWitnessFailureCode::TargetUnavailable, None));
    }
    if crate::win32::resolve_uwp_app_pid(expected_pid, hwnd).is_some() {
        return Err(pending.fail(KeyboardWitnessFailureCode::TargetUnsupported, None));
    }
    if crate::input::post_message_blocked_by_uipi(hwnd).is_some() {
        return Err(pending.fail(KeyboardWitnessFailureCode::PreparationFailed, None));
    }
    let key_vk = key_name_to_vk(key).map_err(|_| {
        pending
            .clone()
            .fail(KeyboardWitnessFailureCode::PreparationFailed, None)
    })?;
    let mod_vks: Vec<VIRTUAL_KEY> = modifiers.iter().filter_map(|m| modifier_vk(m)).collect();
    let mut events: Vec<INPUT> = Vec::with_capacity(mod_vks.len() * 2 + 2);
    for mvk in &mod_vks {
        events.push(key_input(*mvk, false));
    }
    events.push(key_input(key_vk, false));
    events.push(key_input(key_vk, true));
    for mvk in mod_vks.iter().rev() {
        events.push(key_input(*mvk, true));
    }
    // Current standard chord vocabulary has at most four modifiers. Bound the
    // diagnostic count and reject malformed wider requests before activation.
    if events.len() > 64 {
        return Err(pending.fail(KeyboardWitnessFailureCode::PreparationFailed, None));
    }
    let value_before_focus = std::cell::RefCell::new(None::<String>);
    let result = with_confirmed_foreground(
        target,
        "retained key delivery",
        || {
            // This optional provider observation is private and does not actuate.
            // Equality spans the existing focus callback, not a causal attribution
            // to SetFocus alone; activation happened before this observation.
            *value_before_focus.borrow_mut() = retained.keyboard_value_before_focus();
            focus()
        },
        || {
            dispatch_retained_key_once(
                pending.clone(),
                || read_keyboard_native_witness(hwnd, expected_pid),
                || {
                    let private_value = value_before_focus.borrow_mut().take();
                    let primitive = retained.keyboard_focus_witness(
                        hwnd,
                        expected_pid,
                        private_value.as_deref(),
                    );
                    drop(private_value); // No private value survives the native-only phase.
                    primitive
                },
                |uia| read_final_keyboard_witness(hwnd, expected_pid, uia),
                || unsafe { SendInput(&events, std::mem::size_of::<INPUT>() as i32) },
                events.len() as u32,
            )
            .map_err(anyhow::Error::new)
        },
    );
    result.map_err(|error| match error.downcast::<KeyboardWitnessFailure>() {
        Ok(failure) => failure,
        Err(_) => pending.fail(KeyboardWitnessFailureCode::PreparationFailed, None),
    })
}

/// Foreground-delivery text entry: the `delivery_mode:"foreground"` rung for
/// `type_text`. Symmetric with [`send_key_synthesized`] — briefly fronts the
/// target, types `text` as SendInput Unicode (`KEYEVENTF_UNICODE`, so every
/// codepoint lands regardless of keyboard layout), then restores the prior
/// foreground. This is only reached on the explicit `delivery_mode:"foreground"`
/// rung (background never fronts); it does NOT silently fall back to
/// PostMessage: if the foreground swap is rejected
/// (daemon not at UIAccess integrity) it bails with the same diagnostic
/// `send_key_synthesized` returns, so the caller gets an honest error instead
/// of a false success. Required for VCL/LibreOffice document grids and other
/// targets where PostMessage WM_CHAR is silently dropped.
pub fn send_text_synthesized(hwnd: u64, text: &str) -> Result<()> {
    send_text_synthesized_after_focus(hwnd, text, || Ok(()))
}

/// Foreground Unicode delivery with child focus established after exact
/// top-level activation and before `SendInput`.
pub fn send_text_synthesized_after_focus(
    hwnd: u64,
    text: &str,
    focus: impl FnOnce() -> Result<()>,
) -> Result<()> {
    let target = HWND(hwnd as *mut _);
    if target.0.is_null() {
        bail!("invalid target hwnd");
    }
    if let Some(msg) = crate::input::post_message_blocked_by_uipi(hwnd) {
        bail!(msg);
    }
    // Build the key-event sequence. Printable codepoints go through as Unicode
    // packets (KEYEVENTF_UNICODE, wVk=0, wScan=code unit), but line breaks are
    // mapped to a real VK_RETURN keystroke — mirroring the background path
    // (`post_enter_keystroke`) — because terminals and rich editors honour an
    // Enter key event, not a raw `\r`/`\n` Unicode packet. `\r\n` collapses to
    // a single Return (the `\r` emits the Enter; the following `\n` is silent).
    let return_vk = windows::Win32::UI::Input::KeyboardAndMouse::VK_RETURN;
    let mut events: Vec<INPUT> = Vec::with_capacity(text.len() * 2);
    let mut prev_was_cr = false;
    for ch in text.chars() {
        match ch {
            '\n' if prev_was_cr => {
                prev_was_cr = false;
            }
            '\n' | '\r' => {
                events.push(key_input(return_vk, false));
                events.push(key_input(return_vk, true));
                prev_was_cr = ch == '\r';
            }
            _ => {
                prev_was_cr = false;
                let mut buf = [0u16; 2];
                for unit in ch.encode_utf16(&mut buf) {
                    events.push(unicode_key_input(*unit, false));
                    events.push(unicode_key_input(*unit, true));
                }
            }
        }
    }
    if events.is_empty() {
        return Ok(());
    }

    with_confirmed_foreground(target, "text delivery", focus, || unsafe {
        let sent = SendInput(&events, std::mem::size_of::<INPUT>() as i32);
        if sent as usize != events.len() {
            bail!(
                "SendInput inserted only {sent} of {} key events. Likely cause: \
                 the daemon is not at UIAccess integrity, so SetForegroundWindow \
                 was rejected and the events landed on the wrong window.",
                events.len()
            );
        }
        Ok(())
    })
}

fn wait_for_exact_foreground(target: HWND, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if unsafe { GetForegroundWindow() } == target {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        sleep(Duration::from_millis(10));
    }
}

/// Run one system-queue input transaction while `target`, or a live visible
/// same-process window whose owner chain reaches `target`, is foreground.
/// Foreground and optional child focus are checked against
/// external Win32/UIA state instead of inferred from API return values or a
/// fixed settle delay. The prior foreground is restored on every body/focus
/// result after activation succeeds.
pub(crate) fn with_confirmed_foreground<T>(
    target: HWND,
    operation: &str,
    focus: impl FnOnce() -> Result<()>,
    body: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let previous = unsafe { GetForegroundWindow() };
    let _ = unsafe { crate::input::force_foreground_assisted(target) };
    if !wait_for_exact_foreground(target, Duration::from_millis(500)) {
        let actual = unsafe { GetForegroundWindow() };
        if !previous.0.is_null() && previous != target {
            let _ = unsafe { SetForegroundWindow(previous) };
        }
        bail!(
            "foreground_unavailable: Windows did not confirm exact target HWND {:?} for {operation} \
             within 500 ms (actual foreground HWND {:?}). Route the request through the \
             UIAccess-manifested cua-driver-uia worker; no input was sent.",
            target.0,
            actual.0
        );
    }

    let Some(foreground_target) = crate::win32::capture_foreground_target(target.0 as usize as u64)
    else {
        if !previous.0.is_null() && previous != target {
            let _ = unsafe { SetForegroundWindow(previous) };
        }
        bail!(
            "foreground_unavailable: exact target HWND {:?} disappeared before {operation}; \
             no input was sent",
            target.0
        );
    };

    let result = (|| {
        focus()?;
        let deadline = Instant::now() + Duration::from_millis(250);
        let actual = loop {
            let actual = unsafe { GetForegroundWindow() };
            if crate::win32::foreground_matches_target_or_owned_window(
                foreground_target,
                actual.0 as usize as u64,
            ) {
                break actual;
            }
            if Instant::now() >= deadline {
                break actual;
            }
            sleep(Duration::from_millis(10));
        };
        if !crate::win32::foreground_matches_target_or_owned_window(
            foreground_target,
            actual.0 as usize as u64,
        ) {
            bail!(
                "foreground_unavailable: exact target HWND {:?} or a verified same-process \
                 owned window was not foreground while preparing {operation} \
                 (actual foreground HWND {:?}); no input was sent",
                target.0,
                actual.0
            );
        }
        body()
    })();

    // Give the target message loop a bounded opportunity to consume the
    // inserted sequence before restoring the user's prior foreground.
    if result.is_ok() {
        sleep(Duration::from_millis(40));
    }
    if !previous.0.is_null() && previous != target {
        let _ = unsafe { SetForegroundWindow(previous) };
    }
    result
}

/// Build a single Unicode keyboard INPUT struct for one UTF-16 code unit,
/// either down (`up = false`) or up (`up = true`). Used by
/// [`send_text_synthesized`].
fn unicode_key_input(unit: u16, up: bool) -> INPUT {
    let mut flags = KEYEVENTF_UNICODE;
    if up {
        flags |= KEYEVENTF_KEYUP;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0),
                wScan: unit,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// Build a single keyboard INPUT struct for `vk`, either down (`up = false`)
/// or up (`up = true`). Uses scancode + EXTENDEDKEY where applicable so the
/// target sees a hardware-like keystroke.
fn key_input(vk: VIRTUAL_KEY, up: bool) -> INPUT {
    let scan = unsafe { MapVirtualKeyW(vk.0 as u32, MAPVK_VK_TO_VSC) } as u16;
    let mut flags: KEYBD_EVENT_FLAGS = KEYBD_EVENT_FLAGS(0);
    // Scancode is more reliable than VK for some apps. EXTENDEDKEY flag
    // makes arrow / nav / right-side modifier keys work correctly.
    if scan != 0 {
        flags |= KEYEVENTF_SCANCODE;
    }
    if is_extended(vk) {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    if up {
        flags |= KEYEVENTF_KEYUP;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: if scan != 0 { VIRTUAL_KEY(0) } else { vk },
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn modifier_vk(name: &str) -> Option<VIRTUAL_KEY> {
    use windows::Win32::UI::Input::KeyboardAndMouse::*;
    match name.to_lowercase().as_str() {
        "ctrl" | "control" => Some(VK_CONTROL),
        "shift" => Some(VK_SHIFT),
        "alt" | "menu" | "option" => Some(VK_MENU),
        "win" | "meta" | "windows" | "cmd" | "command" => Some(VK_LWIN),
        _ => None,
    }
}

/// Build the SendInput `INPUT` events that HOLD the named modifier keys around a
/// pointer action: `(downs, ups)` where `downs` presses each modifier (in order)
/// and `ups` releases them (reverse order). A pointer path emits `downs`, does
/// the click, then emits `ups`. Unknown modifier names are skipped. Empty input
/// → two empty vecs (no-op). Lets the mouse SendInput path hold cmd/shift/alt/
/// ctrl exactly like `send_key_synthesized` does for keystrokes.
pub fn modifier_hold_inputs(modifiers: &[&str]) -> (Vec<INPUT>, Vec<INPUT>) {
    let mod_vks: Vec<VIRTUAL_KEY> = modifiers.iter().filter_map(|m| modifier_vk(m)).collect();
    let downs: Vec<INPUT> = mod_vks.iter().map(|v| key_input(*v, false)).collect();
    let ups: Vec<INPUT> = mod_vks.iter().rev().map(|v| key_input(*v, true)).collect();
    (downs, ups)
}

fn is_extended(vk: VIRTUAL_KEY) -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::*;
    matches!(
        vk,
        VK_DELETE
            | VK_INSERT
            | VK_HOME
            | VK_END
            | VK_PRIOR
            | VK_NEXT
            | VK_UP
            | VK_DOWN
            | VK_LEFT
            | VK_RIGHT
            | VK_RCONTROL
            | VK_RMENU
            | VK_RWIN
            | VK_NUMLOCK
            | VK_SNAPSHOT
    )
}

fn key_name_to_vk(key: &str) -> Result<VIRTUAL_KEY> {
    use windows::Win32::UI::Input::KeyboardAndMouse::*;
    // Windows reserves the ASCII values themselves as the virtual-key codes
    // for A-Z and 0-9. Resolve the documented alphanumeric vocabulary
    // directly so it does not depend on the daemon thread having a current
    // keyboard layout, which VkKeyScanW requires.
    if let Some(vk) = crate::keycodes::ascii_alphanumeric_virtual_key_code(key) {
        return Ok(VIRTUAL_KEY(vk));
    }

    let vk = match key.to_lowercase().as_str() {
        "enter" | "return" => VK_RETURN,
        "tab" => VK_TAB,
        "escape" | "esc" => VK_ESCAPE,
        "space" | " " => VK_SPACE,
        "backspace" => VK_BACK,
        "delete" | "del" => VK_DELETE,
        "insert" | "ins" => VK_INSERT,
        "home" => VK_HOME,
        "end" => VK_END,
        "pageup" | "pgup" => VK_PRIOR,
        "pagedown" | "pgdn" => VK_NEXT,
        "up" => VK_UP,
        "down" => VK_DOWN,
        "left" => VK_LEFT,
        "right" => VK_RIGHT,
        "f1" => VK_F1,
        "f2" => VK_F2,
        "f3" => VK_F3,
        "f4" => VK_F4,
        "f5" => VK_F5,
        "f6" => VK_F6,
        "f7" => VK_F7,
        "f8" => VK_F8,
        "f9" => VK_F9,
        "f10" => VK_F10,
        "f11" => VK_F11,
        "f12" => VK_F12,
        "ctrl" | "control" => VK_CONTROL,
        "shift" => VK_SHIFT,
        "alt" => VK_MENU,
        "win" | "windows" | "meta" | "command" | "cmd" => VK_LWIN,
        "capslock" => VK_CAPITAL,
        "numlock" => VK_NUMLOCK,
        _ => {
            // Single printable character.
            let ch = key
                .chars()
                .next()
                .ok_or_else(|| anyhow::anyhow!("Empty key name"))?;
            // VkKeyScanW returns VK in low byte.
            let vk_scan =
                unsafe { windows::Win32::UI::Input::KeyboardAndMouse::VkKeyScanW(ch as u16) };
            if vk_scan == -1i16 as u16 as i16 {
                bail!("Unknown key: {key}");
            }
            VIRTUAL_KEY((vk_scan & 0xFF) as u16)
        }
    };
    Ok(vk)
}

#[cfg(test)]
mod keyboard_focus_witness_tests {
    use super::*;
    use crate::uia::cache::{KeyboardUiaFocusWitness, KeyboardUiaWitnessError};
    use std::cell::{Cell, RefCell};

    fn native() -> KeyboardNativeWitness {
        KeyboardNativeWitness {
            target: 10,
            target_thread: 2,
            target_pid: 3,
            root: 10,
            foreground: 10,
            foreground_thread: 2,
            active: 10,
            focus: 11,
            focus_thread: 2,
            focus_pid: 3,
        }
    }
    fn uia() -> KeyboardUiaFocusWitness {
        KeyboardUiaFocusWitness {
            has_focus: true,
            focused_equals_target: true,
            process_matches: true,
            own_native_hwnd: 11,
            nearest_native_hwnd: 11,
            value_changed_on_focus: None,
            scope: None,
        }
    }
    fn pending() -> KeyboardFocusWitness {
        KeyboardFocusWitness::pending(KeyboardElementReferenceKind::Token)
    }
    fn dispatch_with_uia(
        value: KeyboardUiaFocusWitness,
        calls: &Cell<u32>,
    ) -> Result<KeyboardFocusWitness, KeyboardWitnessFailure> {
        dispatch_retained_key_once(
            pending(),
            || Ok(native()),
            || Ok(value),
            |_| Ok((native(), KeyboardNativeCorrelation::ExactNativeChild)),
            || {
                calls.set(calls.get() + 1);
                2
            },
            2,
        )
    }
    fn assert_not_inserted(error: KeyboardWitnessFailure, calls: &Cell<u32>) {
        assert!(!error.witness.input_attempted);
        assert_eq!(error.witness.inserted_event_count, 0);
        assert_eq!(calls.get(), 0);
    }
    #[test]
    fn production_seam_positive_orders_native_uia_native_then_one_insert() {
        let phases = RefCell::new(Vec::new());
        let result = dispatch_retained_key_once(
            pending(),
            || {
                phases.borrow_mut().push("before");
                Ok(native())
            },
            || {
                phases.borrow_mut().push("uia");
                Ok(uia())
            },
            |_| {
                phases.borrow_mut().push("final");
                Ok((native(), KeyboardNativeCorrelation::ExactNativeChild))
            },
            || {
                phases.borrow_mut().push("insert");
                2
            },
            2,
        )
        .unwrap();
        assert_eq!(*phases.borrow(), vec!["before", "uia", "final", "insert"]);
        assert!(result.input_attempted);
        assert_eq!(result.inserted_event_count, 2);
        assert_eq!(result.native_focus_stable, Some(true));
    }
    #[test]
    fn has_focus_false_refuses_before_sender_or_final_callback() {
        let calls = Cell::new(0);
        let mut value = uia();
        value.has_focus = false;
        let error = dispatch_retained_key_once(
            pending(),
            || Ok(native()),
            || Ok(value),
            |_| panic!("negative UIA must not reach final callback"),
            || {
                calls.set(1);
                2
            },
            2,
        )
        .unwrap_err();
        assert_eq!(error.witness.uia_has_keyboard_focus, Some(false));
        assert_not_inserted(error, &calls);
    }
    #[test]
    fn provider_has_focus_true_but_exact_object_differs_refuses() {
        let calls = Cell::new(0);
        let mut value = uia();
        value.focused_equals_target = false;
        let error = dispatch_with_uia(value, &calls).unwrap_err();
        assert_eq!(error.witness.uia_focused_element_equals_target, Some(false));
        assert_not_inserted(error, &calls);
    }
    #[test]
    fn same_native_root_does_not_admit_foreign_uia_process() {
        let calls = Cell::new(0);
        let mut value = uia();
        value.process_matches = false;
        let error = dispatch_with_uia(value, &calls).unwrap_err();
        assert_eq!(error.witness.uia_process_matches_target, Some(false));
        assert_not_inserted(error, &calls);
    }
    #[test]
    fn provider_error_preserves_hresult_only_and_no_insertion() {
        let calls = Cell::new(0);
        let error = dispatch_retained_key_once(
            pending(),
            || Ok(native()),
            || Err(KeyboardUiaWitnessError::Unavailable(Some(-2147220991))),
            |_| panic!("unreadable provider must stop"),
            || {
                calls.set(1);
                2
            },
            2,
        )
        .unwrap_err();
        assert_eq!(error.witness.provider_failure_code, Some(-2147220991));
        assert_not_inserted(error, &calls);
    }
    #[test]
    fn gui_thread_query_unreadable_stops_before_provider_and_sender() {
        let calls = Cell::new(0);
        let error = dispatch_retained_key_once(
            pending(),
            || Err(KeyboardWitnessFailureCode::NativeFocusUnavailable.into()),
            || panic!("no GUI witness must stop"),
            |_| panic!("no final read"),
            || {
                calls.set(1);
                2
            },
            2,
        )
        .unwrap_err();
        assert_not_inserted(error, &calls);
    }
    #[test]
    fn native_focus_outside_boundary_refuses_before_sender() {
        let calls = Cell::new(0);
        let error = dispatch_retained_key_once(
            pending(),
            || Ok(native()),
            || Ok(uia()),
            |_| Err(KeyboardWitnessFailureCode::NativeFocusMismatch.into()),
            || {
                calls.set(1);
                2
            },
            2,
        )
        .unwrap_err();
        assert_not_inserted(error, &calls);
    }
    #[test]
    fn native_focus_change_after_provider_reads_refuses() {
        let calls = Cell::new(0);
        let mut final_native = native();
        final_native.focus = 12;
        let error = dispatch_retained_key_once(
            pending(),
            || Ok(native()),
            || Ok(uia()),
            |_| Ok((final_native, KeyboardNativeCorrelation::ExactNativeChild)),
            || {
                calls.set(1);
                2
            },
            2,
        )
        .unwrap_err();
        assert_eq!(error.witness.native_focus_stable, Some(false));
        assert_not_inserted(error, &calls);
    }
    #[test]
    fn provider_teardown_finishes_before_final_native_gate() {
        let cleaned = Cell::new(false);
        let calls = Cell::new(0);
        struct ProviderOwner<'a>(&'a Cell<bool>);
        impl Drop for ProviderOwner<'_> {
            fn drop(&mut self) {
                self.0.set(true);
            }
        }
        let error = dispatch_retained_key_once(
            pending(),
            || Ok(native()),
            || {
                let _owner = ProviderOwner(&cleaned);
                Ok(uia())
            },
            |_| {
                assert!(cleaned.get());
                Err(KeyboardWitnessFailureCode::NativeFocusChanged.into())
            },
            || {
                calls.set(1);
                2
            },
            2,
        )
        .unwrap_err();
        assert_not_inserted(error, &calls);
    }
    #[test]
    fn shared_ancestor_remains_distinct_without_consumption_claim() {
        let result = dispatch_retained_key_once(
            pending(),
            || Ok(native()),
            || Ok(uia()),
            |_| Ok((native(), KeyboardNativeCorrelation::SharedAncestor)),
            || 2,
            2,
        )
        .unwrap();
        let json: serde_json::Value = serde_json::from_str(
            result
                .diagnostic_text()
                .strip_prefix(KEYBOARD_FOCUS_WITNESS_MARKER)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(json["nativeElementCorrelation"], "shared_ancestor");
        assert!(json.get("effect").is_none());
    }
    #[test]
    fn partial_input_is_one_attempt_not_refusal_or_retry() {
        let calls = Cell::new(0);
        let error = dispatch_retained_key_once(
            pending(),
            || Ok(native()),
            || Ok(uia()),
            |_| Ok((native(), KeyboardNativeCorrelation::ExactNativeChild)),
            || {
                calls.set(calls.get() + 1);
                1
            },
            2,
        )
        .unwrap_err();
        assert_eq!(calls.get(), 1);
        assert!(error.witness.input_attempted);
        assert_eq!(error.witness.inserted_event_count, 1);
        assert!(matches!(
            error.witness.failure_code,
            Some(KeyboardWitnessFailureCode::InputPartial)
        ));
    }
    #[test]
    fn optional_value_equality_never_changes_admission_and_is_content_free() {
        for changed in [None, Some(false), Some(true)] {
            let calls = Cell::new(0);
            let mut value = uia();
            value.value_changed_on_focus = changed;
            let result = dispatch_with_uia(value, &calls).unwrap();
            assert_eq!(calls.get(), 1);
            assert_eq!(result.value_changed_on_focus, changed);
            let text = result.diagnostic_text();
            let json: serde_json::Value =
                serde_json::from_str(text.strip_prefix(KEYBOARD_FOCUS_WITNESS_MARKER).unwrap())
                    .unwrap();
            assert_eq!(json.as_object().unwrap().len(), 18);
            assert!(text.len() < KEYBOARD_FOCUS_WITNESS_MARKER.len() + 2048);
            for forbidden in [
                "target",
                "pid",
                "root",
                "focus",
                "value",
                "runtimeId",
                "title",
                "name",
                "key",
            ] {
                assert!(json.get(forbidden).is_none());
            }
        }
    }
}

/// Native GUI fixture uses the production gate with a fake insertion counter;
/// no actual keyboard input is injected into either fixture or live desktop.
#[cfg(test)]
pub(crate) fn dispatch_retained_key_for_owned_fixture(
    hwnd: u64,
    expected_pid: u32,
    retained: &crate::uia::cache::RetainedElement,
    insert_calls: &std::cell::Cell<u32>,
) -> Result<KeyboardFocusWitness, KeyboardWitnessFailure> {
    dispatch_retained_key_once(
        KeyboardFocusWitness::pending(KeyboardElementReferenceKind::Token),
        || read_keyboard_native_witness(hwnd, expected_pid),
        || retained.keyboard_focus_witness(hwnd, expected_pid, None),
        |uia| read_final_keyboard_witness(hwnd, expected_pid, uia),
        || {
            insert_calls.set(insert_calls.get() + 1);
            2
        },
        2,
    )
}

#[cfg(test)]
mod virtual_key_scope_tests {
    use super::*;
    use crate::uia::cache::{InputUiaOwnership, InputUiaScopeWitness, KeyboardUiaFocusWitness};
    use std::cell::Cell;
    fn native() -> KeyboardNativeWitness {
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
    fn scope() -> InputUiaScopeWitness {
        InputUiaScopeWitness {
            own_native_hwnd: 0,
            nearest_native_hwnd: 12,
            provider_pid: 77,
            anchor_pid: 33,
            anchor_thread: 22,
            anchor_parent: 10,
            anchor_root: 10,
            ownership: InputUiaOwnership::VirtualDocument,
        }
    }
    fn uia() -> KeyboardUiaFocusWitness {
        KeyboardUiaFocusWitness {
            has_focus: true,
            focused_equals_target: true,
            process_matches: true,
            own_native_hwnd: 0,
            nearest_native_hwnd: 12,
            value_changed_on_focus: None,
            scope: Some(scope()),
        }
    }
    fn actual() -> KeyboardNativeScopeWitness {
        KeyboardNativeScopeWitness {
            hwnd: 12,
            parent: 10,
            root: 10,
            thread: 22,
            pid: 33,
        }
    }
    fn dispatch(
        actual: KeyboardNativeScopeWitness,
        refreshed: InputUiaScopeWitness,
        focus_within_anchor: bool,
        calls: &Cell<u32>,
    ) -> Result<KeyboardFocusWitness, KeyboardWitnessFailure> {
        dispatch_retained_key_once(
            KeyboardFocusWitness::pending(KeyboardElementReferenceKind::Token),
            || Ok(native()),
            || Ok(uia()),
            |uia| {
                if !keyboard_native_scope_matches(native(), uia, scope(), refreshed, actual) {
                    return Err(KeyboardWitnessFailureCode::NativeFocusMismatch
                        .at(KeyboardNativeRejection::AnchorIdentity));
                }
                if !keyboard_native_focus_relation_admitted(
                    native(),
                    uia,
                    actual,
                    focus_within_anchor,
                ) {
                    return Err(KeyboardWitnessFailureCode::NativeFocusMismatch
                        .at(KeyboardNativeRejection::AnchorFocusRelation));
                }
                Ok((native(), KeyboardNativeCorrelation::SharedAncestor))
            },
            || {
                calls.set(calls.get() + 1);
                2
            },
            2,
        )
    }
    fn assert_zero(
        result: Result<KeyboardFocusWitness, KeyboardWitnessFailure>,
        calls: &Cell<u32>,
    ) {
        let error = result.unwrap_err();
        assert!(!error.witness.input_attempted);
        assert_eq!(error.witness.inserted_event_count, 0);
        assert_eq!(calls.get(), 0);
    }
    #[test]
    fn proved_virtual_child_process_has_one_insertion_and_same_closed_wire() {
        let calls = Cell::new(0);
        let witness = dispatch(actual(), scope(), true, &calls).unwrap();
        assert_eq!(calls.get(), 1);
        assert_eq!(witness.uia_process_matches_target, Some(true));
        assert_eq!(witness.inserted_event_count, 2);
        let json = serde_json::to_value(&witness).unwrap();
        assert_eq!(json.as_object().unwrap().len(), 18);
        assert_eq!(json["nativeElementCorrelation"], "shared_ancestor");
        assert!(json.get("providerPid").is_none());
        assert!(json.get("ownership").is_none());
    }
    #[test]
    fn wrong_root_native_scope_stops_before_sender() {
        let calls = Cell::new(0);
        let mut a = actual();
        a.root = 20;
        assert_zero(dispatch(a, scope(), true, &calls), &calls);
    }
    #[test]
    fn same_root_sibling_focus_stops_before_sender() {
        let calls = Cell::new(0);
        assert_zero(dispatch(actual(), scope(), false, &calls), &calls);
    }
    #[test]
    fn same_leaf_hwnd_intermediate_focus_reparent_stops_before_sender() {
        let calls = Cell::new(0);
        let error = dispatch(actual(), scope(), false, &calls).unwrap_err();
        assert!(error
            .to_string()
            .contains("native_subreason=anchor_focus_relation;"));
        assert_zero(Err(error), &calls);
    }
    #[test]
    fn changed_anchor_parent_stops_before_sender() {
        let calls = Cell::new(0);
        let mut refreshed = scope();
        refreshed.anchor_parent = 99;
        assert_zero(dispatch(actual(), refreshed, true, &calls), &calls);
    }
    #[test]
    fn native_anchor_reuse_with_different_owner_stops_before_sender() {
        let calls = Cell::new(0);
        let mut a = actual();
        a.pid = 88;
        assert_zero(dispatch(a, scope(), true, &calls), &calls);
    }
    #[test]
    fn direct_native_path_still_requires_frame_process_focus() {
        let mut expected = scope();
        expected.ownership = InputUiaOwnership::DirectProcess;
        assert!(!keyboard_native_scope_matches(
            native(),
            uia(),
            expected,
            expected,
            actual()
        ));
    }
    fn parent_native(parent: u64) -> KeyboardNativeWitness {
        KeyboardNativeWitness {
            focus: parent,
            ..native()
        }
    }
    fn parent_dispatch(
        before: KeyboardNativeWitness,
        final_native: KeyboardNativeWitness,
        semantic: KeyboardUiaFocusWitness,
        refreshed: InputUiaScopeWitness,
        actual: KeyboardNativeScopeWitness,
        calls: &Cell<u32>,
    ) -> Result<KeyboardFocusWitness, KeyboardWitnessFailure> {
        dispatch_retained_key_once(
            KeyboardFocusWitness::pending(KeyboardElementReferenceKind::Token),
            || Ok(before),
            || Ok(semantic),
            |uia| {
                let expected = uia.scope.ok_or_else(|| {
                    KeyboardWitnessFailureCode::NativeFocusUnavailable
                        .at(KeyboardNativeRejection::AnchorUnavailable)
                })?;
                if !keyboard_native_scope_matches(final_native, uia, expected, refreshed, actual)
                    || !keyboard_native_focus_relation_admitted(final_native, uia, actual, false)
                {
                    return Err(KeyboardWitnessFailureCode::NativeFocusMismatch
                        .at(KeyboardNativeRejection::AnchorFocusRelation));
                }
                Ok((final_native, KeyboardNativeCorrelation::SharedAncestor))
            },
            || {
                calls.set(calls.get() + 1);
                2
            },
            2,
        )
    }
    #[test]
    fn virtual_immediate_root_parent_correlates_once_without_wire_expansion() {
        let calls = Cell::new(0);
        let native = parent_native(10);
        let result = parent_dispatch(native, native, uia(), scope(), actual(), &calls).unwrap();
        assert_eq!(calls.get(), 1);
        assert_eq!(result.inserted_event_count, 2);
        assert_eq!(
            result.native_element_correlation,
            KeyboardNativeCorrelation::SharedAncestor
        );
        let wire = serde_json::to_value(result).unwrap();
        assert_eq!(wire.as_object().unwrap().len(), 18);
        assert_eq!(wire["nativeElementCorrelation"], "shared_ancestor");
        assert!(wire.get("providerPid").is_none());
    }
    #[test]
    fn virtual_immediate_nonroot_parent_correlates_once() {
        let calls = Cell::new(0);
        let native = parent_native(11);
        let mut expected = scope();
        expected.anchor_parent = 11;
        let mut semantic = uia();
        semantic.scope = Some(expected);
        parent_dispatch(
            native,
            native,
            semantic,
            expected,
            KeyboardNativeScopeWitness {
                parent: 11,
                ..actual()
            },
            &calls,
        )
        .unwrap();
        assert_eq!(calls.get(), 1);
    }
    #[test]
    fn virtual_parent_sibling_and_nonimmediate_root_focus_never_send() {
        for focus in [10, 13, 14] {
            let calls = Cell::new(0);
            let native = parent_native(focus);
            let mut expected = scope();
            expected.anchor_parent = 11;
            let mut semantic = uia();
            semantic.scope = Some(expected);
            assert_zero(
                parent_dispatch(
                    native,
                    native,
                    semantic,
                    expected,
                    KeyboardNativeScopeWitness {
                        parent: 11,
                        ..actual()
                    },
                    &calls,
                ),
                &calls,
            );
        }
    }
    #[test]
    fn virtual_parent_focus_owner_or_thread_mismatch_never_sends() {
        for (pid, thread) in [(0, 22), (33, 0), (44, 22), (33, 44)] {
            let calls = Cell::new(0);
            let native = KeyboardNativeWitness {
                focus_pid: pid,
                focus_thread: thread,
                ..parent_native(10)
            };
            assert_zero(
                parent_dispatch(native, native, uia(), scope(), actual(), &calls),
                &calls,
            );
        }
    }
    #[test]
    fn virtual_parent_semantic_focus_mismatch_or_missing_scope_never_sends() {
        for variant in 0..4 {
            let calls = Cell::new(0);
            let native = parent_native(10);
            let mut semantic = uia();
            match variant {
                0 => semantic.has_focus = false,
                1 => semantic.focused_equals_target = false,
                2 => semantic.process_matches = false,
                _ => semantic.scope = None,
            }
            assert_zero(
                parent_dispatch(native, native, semantic, scope(), actual(), &calls),
                &calls,
            );
        }
    }
    #[test]
    fn virtual_parent_direct_native_root_anchor_and_native_leaf_never_send() {
        for variant in 0..3 {
            let calls = Cell::new(0);
            let mut native = parent_native(10);
            let mut semantic = uia();
            let mut expected = scope();
            let mut actual = actual();
            match variant {
                0 => {
                    expected.ownership = InputUiaOwnership::DirectProcess;
                    expected.provider_pid = 33;
                    native.target_pid = 33;
                }
                1 => {
                    expected.nearest_native_hwnd = 10;
                    semantic.nearest_native_hwnd = 10;
                    actual.hwnd = 10;
                }
                _ => {
                    expected.own_native_hwnd = 12;
                    semantic.own_native_hwnd = 12;
                }
            }
            semantic.scope = Some(expected);
            assert_zero(
                parent_dispatch(native, native, semantic, expected, actual, &calls),
                &calls,
            );
        }
    }
    #[test]
    fn virtual_parent_anchor_identity_or_parent_drift_never_sends() {
        for variant in 0..4 {
            let calls = Cell::new(0);
            let native = parent_native(10);
            let mut refreshed = scope();
            match variant {
                0 => refreshed.anchor_parent = 99,
                1 => refreshed.anchor_pid = 99,
                2 => refreshed.anchor_thread = 99,
                _ => refreshed.anchor_root = 99,
            }
            assert_zero(
                parent_dispatch(native, native, uia(), refreshed, actual(), &calls),
                &calls,
            );
        }
    }
    #[test]
    fn virtual_parent_final_foreground_active_focus_or_owner_drift_never_sends() {
        for variant in 0..4 {
            let calls = Cell::new(0);
            let before = parent_native(10);
            let mut after = before;
            match variant {
                0 => after.foreground_thread += 1,
                1 => after.active = 11,
                2 => after.focus = 11,
                _ => after.focus_pid += 1,
            }
            assert_zero(
                parent_dispatch(before, after, uia(), scope(), actual(), &calls),
                &calls,
            );
        }
    }
    #[test]
    fn primitive_native_subreasons_do_not_expand_or_leak_wire() {
        let reasons = [
            KeyboardNativeRejection::Foreground,
            KeyboardNativeRejection::ForegroundThread,
            KeyboardNativeRejection::GuiThreadQuery,
            KeyboardNativeRejection::ActiveBoundary,
            KeyboardNativeRejection::FocusBoundary,
            KeyboardNativeRejection::FocusThread,
            KeyboardNativeRejection::ForegroundChanged,
            KeyboardNativeRejection::AdmittedTarget,
            KeyboardNativeRejection::AnchorUnavailable,
            KeyboardNativeRejection::AnchorIdentity,
            KeyboardNativeRejection::AnchorFocusRelation,
            KeyboardNativeRejection::NativeSnapshotChanged,
        ];
        for reason in reasons {
            let failure = KeyboardFocusWitness::pending(KeyboardElementReferenceKind::Token)
                .fail_native(KeyboardWitnessFailureCode::NativeFocusMismatch.at(reason));
            assert!(failure
                .to_string()
                .contains(&format!("native_subreason={};", reason.as_str())));
            let json = serde_json::to_value(&failure.witness).unwrap();
            assert_eq!(json.as_object().unwrap().len(), 18);
            assert_eq!(json["inputAttempted"], false);
            assert_eq!(json["insertedEventCount"], 0);
        }
    }
}
