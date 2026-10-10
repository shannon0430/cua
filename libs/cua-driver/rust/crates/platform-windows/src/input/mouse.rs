//! Background mouse injection via PostMessage.
//!
//! All clicks target the **deepest child** HWND at the click point
//! (via ChildWindowFromPointEx), so the message never reaches the top-level
//! chrome that would call SetForegroundWindow in response to WM_LBUTTONDOWN.

use anyhow::{bail, Context, Result};
use std::thread::sleep;
use std::time::Duration;
use windows::Win32::Foundation::{HWND, LPARAM, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::{ClientToScreen, ScreenToClient};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL,
    MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP,
    MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK,
    MOUSEEVENTF_WHEEL, MOUSEINPUT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    ChildWindowFromPointEx, GetAncestor, GetClassLongPtrW, GetCursorPos, GetForegroundWindow,
    GetSystemMetrics, GetWindowLongPtrW, PostMessageW, SetCursorPos, SetWindowPos, CS_DBLCLKS,
    CWP_SKIPDISABLED, CWP_SKIPINVISIBLE, CWP_SKIPTRANSPARENT, GA_ROOT, GCL_STYLE, GWL_EXSTYLE,
    HWND_NOTOPMOST, HWND_TOP, HWND_TOPMOST, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN,
    SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, WM_LBUTTONDBLCLK,
    WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDBLCLK, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEMOVE,
    WM_RBUTTONDBLCLK, WM_RBUTTONDOWN, WM_RBUTTONUP, WS_EX_TOPMOST,
};

const MK_LBUTTON: u32 = 0x0001;
const MK_MBUTTON: u32 = 0x0010;
const MK_RBUTTON: u32 = 0x0002;

const CLICK_DELAY_MS: u64 = 35;

fn posted_press_message(down: u32, double: u32, click_index: usize, wants_double: bool) -> u32 {
    if wants_double && click_index % 2 == 1 {
        double
    } else {
        down
    }
}

/// Walk from `root` down to the deepest visible child that contains
/// `screen_pt`, mirroring trope-cua's DeepestChildFromScreenPoint.
///
/// Posting to the deepest child avoids the top-level window responding to
/// WM_LBUTTONDOWN by activating itself (focus-steal).
fn deepest_child(root: HWND, screen_pt: POINT) -> (HWND, POINT) {
    let mut current = root;
    for _ in 0..16 {
        let mut client = screen_pt;
        unsafe {
            let _ = ScreenToClient(current, &mut client);
        }
        let child = unsafe {
            ChildWindowFromPointEx(
                current,
                client,
                CWP_SKIPINVISIBLE | CWP_SKIPDISABLED | CWP_SKIPTRANSPARENT,
            )
        };
        // No deeper child, or same window, or outside root's subtree.
        if child.is_invalid() || child == current {
            break;
        }
        // Verify the child is actually a descendant of root.
        let is_child = unsafe { windows::Win32::UI::WindowsAndMessaging::IsChild(root, child) };
        if !is_child.as_bool() && child != root {
            break;
        }
        current = child;
    }
    // Return child-local client coordinates for `current`.
    let mut client = screen_pt;
    unsafe {
        let _ = ScreenToClient(current, &mut client);
    }
    (current, client)
}

/// Post a click at **client-area** coordinates of `root_hwnd`.
///
/// Resolves to the deepest child HWND at the click point before posting,
/// so top-level browser/chrome windows don't activate in response.
pub fn post_click(root: u64, x: i32, y: i32, count: usize, button: &str) -> Result<()> {
    let root_hwnd = HWND(root as *mut _);

    // Convert root-local client → screen.
    let mut screen_pt = POINT { x, y };
    unsafe {
        let _ = ClientToScreen(root_hwnd, &mut screen_pt);
    }

    // Find deepest child and its local client coordinates.
    let (target, client) = deepest_child(root_hwnd, screen_pt);

    post_click_on(target, client.x, client.y, count, button)
}

/// Post a click at **screen** coordinates, resolving the deepest child of
/// `root_hwnd` at that point.  Call this when you already have screen coords.
pub fn post_click_screen(root: u64, sx: i32, sy: i32, count: usize, button: &str) -> Result<()> {
    let root_hwnd = HWND(root as *mut _);
    let screen_pt = POINT { x: sx, y: sy };
    let (target, client) = deepest_child(root_hwnd, screen_pt);
    post_click_on(target, client.x, client.y, count, button)
}

/// Internal: post click messages to `hwnd` using its own client coordinates.
fn post_click_on(hwnd: HWND, x: i32, y: i32, count: usize, button: &str) -> Result<()> {
    // UIPI check — Medium-IL daemon → High-IL target silently drops mouse
    // messages just like keyboard ones. Surface an actionable error before
    // PostMessage returns its misleading TRUE. See post_message_blocked_by_uipi.
    if let Some(msg) = crate::input::post_message_blocked_by_uipi(hwnd.0 as u64) {
        anyhow::bail!(msg);
    }

    let (down_msg, double_msg, up_msg, mk_flag) = match button {
        "right" => (WM_RBUTTONDOWN, WM_RBUTTONDBLCLK, WM_RBUTTONUP, MK_RBUTTON),
        "middle" => (WM_MBUTTONDOWN, WM_MBUTTONDBLCLK, WM_MBUTTONUP, MK_MBUTTON),
        _ => (WM_LBUTTONDOWN, WM_LBUTTONDBLCLK, WM_LBUTTONUP, MK_LBUTTON),
    };
    let lparam = make_lparam(x, y);
    let wdown = WPARAM(mk_flag as usize);
    let wup = WPARAM(0);
    let wants_double = unsafe { (GetClassLongPtrW(hwnd, GCL_STYLE) as u32 & CS_DBLCLKS.0) != 0 };
    let prev_fg = unsafe { GetForegroundWindow() };
    let target_root = unsafe {
        let root = GetAncestor(hwnd, GA_ROOT);
        if root.0.is_null() {
            hwnd
        } else {
            root
        }
    };

    // Posted pointer messages are normally non-activating, but WebView hosts can
    // call SetForegroundWindow from their event handlers. Keep the top-level
    // categorically non-activatable until the complete burst has settled.
    let _noact = crate::input::NoActivateGuard::arm(hwnd);
    for i in 0..count {
        let press_msg = posted_press_message(down_msg, double_msg, i, wants_double);
        unsafe {
            // WM_MOUSEMOVE first so hover state is correct before the click.
            PostMessageW(hwnd, WM_MOUSEMOVE, WPARAM(0), lparam)?;
            // Win32 controls do not infer a double-click from two posted DOWN
            // messages. The second press must use WM_*BUTTONDBLCLK.
            PostMessageW(hwnd, press_msg, wdown, lparam)?;
            sleep(Duration::from_millis(CLICK_DELAY_MS));
            PostMessageW(hwnd, up_msg, wup, lparam)?;
        }
        if i + 1 < count {
            sleep(Duration::from_millis(80));
        }
    }
    sleep(Duration::from_millis(50));
    unsafe {
        if !prev_fg.0.is_null() && prev_fg != target_root && GetForegroundWindow() == target_root {
            crate::input::force_foreground_attached(prev_fg);
            sleep(Duration::from_millis(12));
            crate::input::force_foreground_attached(prev_fg);
        }
    }
    Ok(())
}

/// Post a press-drag-release gesture via PostMessage.
///
/// Coordinates are root-hwnd client-area relative.
pub fn post_drag(
    hwnd: u64,
    from_x: i32,
    from_y: i32,
    to_x: i32,
    to_y: i32,
    duration_ms: u64,
    steps: usize,
    button: &str,
) -> Result<()> {
    let root = HWND(hwnd as *mut _);
    let (down_msg, up_msg, mk_flag) = match button {
        "right" => (WM_RBUTTONDOWN, WM_RBUTTONUP, MK_RBUTTON),
        "middle" => (WM_MBUTTONDOWN, WM_MBUTTONUP, MK_MBUTTON),
        _ => (WM_LBUTTONDOWN, WM_LBUTTONUP, MK_LBUTTON),
    };
    let wparam = WPARAM(mk_flag as usize);
    let steps = steps.max(1);
    let step_delay_ms = if steps > 1 {
        duration_ms / steps as u64
    } else {
        duration_ms
    };

    unsafe {
        PostMessageW(root, down_msg, wparam, make_lparam(from_x, from_y))?;
    }
    sleep(Duration::from_millis(CLICK_DELAY_MS));

    for i in 1..=steps {
        let t = i as f64 / steps as f64;
        let ix = from_x + ((to_x - from_x) as f64 * t).round() as i32;
        let iy = from_y + ((to_y - from_y) as f64 * t).round() as i32;
        unsafe {
            PostMessageW(root, WM_MOUSEMOVE, wparam, make_lparam(ix, iy))?;
        }
        if step_delay_ms > 0 {
            sleep(Duration::from_millis(step_delay_ms));
        }
    }

    unsafe {
        PostMessageW(root, up_msg, WPARAM(0), make_lparam(to_x, to_y))?;
    }
    Ok(())
}

/// Press-drag-release via PostMessage, resolving the **deepest child** at the
/// drag-start screen point and posting in that child's client coordinates.
///
/// `post_drag` (above) posts to the top-level frame, so a child-windowed
/// control (a WinForms `Panel`, a Win32 child canvas, …) never sees the drag —
/// the frame gets messages over a region it doesn't own and ignores them. This
/// variant mirrors `post_click`: it hit-tests down to the deepest descendant
/// under the start point and targets that HWND for the whole gesture (a drag
/// stays within one control), with each point converted to the child's own
/// client space. Endpoints are given in **screen** coordinates.
pub fn post_drag_screen(
    root: u64,
    sx_from: i32,
    sy_from: i32,
    sx_to: i32,
    sy_to: i32,
    duration_ms: u64,
    steps: usize,
    button: &str,
) -> Result<()> {
    let root_hwnd = HWND(root as *mut _);
    let (target, c_from) = deepest_child(
        root_hwnd,
        POINT {
            x: sx_from,
            y: sy_from,
        },
    );
    let mut c_to = POINT { x: sx_to, y: sy_to };
    unsafe {
        let _ = ScreenToClient(target, &mut c_to);
    }
    if let Some(msg) = crate::input::post_message_blocked_by_uipi(target.0 as u64) {
        anyhow::bail!(msg);
    }
    let (down_msg, up_msg, mk_flag) = match button {
        "right" => (WM_RBUTTONDOWN, WM_RBUTTONUP, MK_RBUTTON),
        "middle" => (WM_MBUTTONDOWN, WM_MBUTTONUP, MK_MBUTTON),
        _ => (WM_LBUTTONDOWN, WM_LBUTTONUP, MK_LBUTTON),
    };
    let wparam = WPARAM(mk_flag as usize);
    let steps = steps.max(1);
    let step_delay_ms = if steps > 1 {
        duration_ms / steps as u64
    } else {
        duration_ms
    };
    unsafe {
        // Pre-drag MOUSEMOVE (wParam=0, no buttons down yet) then DOWN at from.
        PostMessageW(
            target,
            WM_MOUSEMOVE,
            WPARAM(0),
            make_lparam(c_from.x, c_from.y),
        )?;
        PostMessageW(target, down_msg, wparam, make_lparam(c_from.x, c_from.y))?;
    }
    sleep(Duration::from_millis(CLICK_DELAY_MS));
    for i in 1..=steps {
        let t = i as f64 / steps as f64;
        let ix = c_from.x + ((c_to.x - c_from.x) as f64 * t).round() as i32;
        let iy = c_from.y + ((c_to.y - c_from.y) as f64 * t).round() as i32;
        unsafe {
            PostMessageW(target, WM_MOUSEMOVE, wparam, make_lparam(ix, iy))?;
        }
        if step_delay_ms > 0 {
            sleep(Duration::from_millis(step_delay_ms));
        }
    }
    unsafe {
        PostMessageW(target, up_msg, WPARAM(0), make_lparam(c_to.x, c_to.y))?;
    }
    Ok(())
}

/// Pack two 16-bit integers into a LPARAM (low word = x, high word = y).
///
/// Delegates the bit-math to [`crate::lparam::pack_xy`] so the
/// receiver-side `GET_X_LPARAM` / `GET_Y_LPARAM` sign-extension contract is
/// covered by cross-platform unit tests (see #1979's audit: the PostMessage
/// path packing was a suspect for the multi-monitor wrong-screen symptom,
/// turned out to be correct, and now has regression coverage).
///
/// On the (currently unreachable) out-of-range path we log + clamp rather
/// than panic — every existing call site passes post-`ScreenToClient`
/// window-local coords that fit in `i16` by construction, but if a future
/// caller passes a raw screen coord on a >32k-px virtual desktop, clamping
/// is at least visible in the log instead of silently wrapping.
fn make_lparam(x: i32, y: i32) -> LPARAM {
    match crate::lparam::pack_xy(x, y) {
        Ok(packed) => LPARAM(packed as isize),
        Err(err) => {
            tracing::warn!(
                target: "click",
                "make_lparam: {err}; clamping to i16 range. \
                 If you see this, the caller is passing non-window-local coords."
            );
            let clamp = |v: i32| v.clamp(i16::MIN as i32, i16::MAX as i32);
            let cx = clamp(x);
            let cy = clamp(y);
            let packed =
                crate::lparam::pack_xy(cx, cy).expect("clamped values always fit in i16 range");
            LPARAM(packed as isize)
        }
    }
}

/// Returns `true` when `hwnd` is a top-level frame of a Chromium-based browser
/// — Edge, Chrome, Brave, Vivaldi, Opera, Chromium, Arc, Thorium, Iridium,
/// Yandex, or any other Chromium-derivative. Matches by window class name,
/// which is stable across versions and consistent across Chromium forks.
///
/// Chromium uses the window class `Chrome_WidgetWin_1` (or `Chrome_WidgetWin_0`
/// for in-process child frames; both should be treated the same way). Electron
/// apps that embed Chromium also use this class, so Electron app coord clicks
/// will route through the SendInput path too — that's intentional, same root
/// cause (#1623).
///
/// Cheap call: one `GetClassNameW` to a 32-char buffer + a `matches!` against
/// the known prefixes. Suitable to call inline in the click dispatch hot path.
pub fn is_chromium_target_window(hwnd: u64) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::GetClassNameW;
    if hwnd == 0 {
        tracing::debug!(target: "click", "is_chromium_target_window: hwnd=0 short-circuit");
        return false;
    }
    let mut buf = [0u16; 64];
    let n = unsafe { GetClassNameW(HWND(hwnd as *mut _), &mut buf) };
    if n <= 0 {
        tracing::debug!(
            target: "click",
            "is_chromium_target_window: GetClassNameW returned {n} for hwnd=0x{hwnd:x}"
        );
        return false;
    }
    let class_name = String::from_utf16_lossy(&buf[..n as usize]);
    // Chromium-family classes. Match by prefix because Chromium suffixes a
    // 0/1 digit; future Chromium forks may use other suffixes.
    let is_chromium = class_name.starts_with("Chrome_WidgetWin_")
        // Electron sometimes uses CefBrowserWindow or similar — be permissive.
        || class_name.starts_with("CefBrowser");
    tracing::debug!(
        target: "click",
        "is_chromium_target_window: hwnd=0x{hwnd:x} class={class_name:?} → {is_chromium}"
    );
    is_chromium
}

/// Return true when `hwnd` hosts a Chromium/WebView2 renderer child even if
/// its own top-level class is framework-specific (for example a Tauri host).
/// Keep this separate from [`is_chromium_target_window`]: embedded WebView2
/// surfaces support some UIA/top-level background routes that direct Chromium
/// frames do not, so delivery policy needs to distinguish the two shapes.
pub fn has_chromium_descendant(hwnd: u64) -> bool {
    use windows::Win32::Foundation::{BOOL, FALSE, LPARAM, TRUE};
    use windows::Win32::UI::WindowsAndMessaging::{EnumChildWindows, GetClassNameW};

    if hwnd == 0 {
        return false;
    }
    struct Scan {
        found: bool,
    }
    unsafe extern "system" fn child_cb(child: HWND, lparam: LPARAM) -> BOOL {
        let scan = &mut *(lparam.0 as *mut Scan);
        let mut buf = [0u16; 64];
        let n = GetClassNameW(child, &mut buf);
        if n > 0 {
            let class = String::from_utf16_lossy(&buf[..n as usize]);
            if class.starts_with("Chrome_WidgetWin_") || class.starts_with("CefBrowser") {
                scan.found = true;
                return FALSE;
            }
        }
        TRUE
    }

    let mut scan = Scan { found: false };
    unsafe {
        let _ = EnumChildWindows(
            HWND(hwnd as *mut _),
            Some(child_cb),
            LPARAM(&mut scan as *mut Scan as isize),
        );
    }
    scan.found
}

/// Click at **screen** coordinates `(sx, sy)` via `SendInput` against the
/// system input queue, briefly focusing `target` so the click lands there.
///
/// Why this exists alongside `post_click_screen`: PostMessage(WM_LBUTTONDOWN)
/// to Chromium-based browsers' top-level frame HWND (or Chrome_RenderWidgetHostHWND
/// descendant) doesn't fire DOM `onclick` / `mousedown` handlers. Chromium's
/// input thread architecture requires events with `SendInput`-queue origin —
/// the same constraint that broke modifier-state hotkey delivery (#1614/#1618)
/// applies to coord clicks on Chromium content (#1623).
///
/// `SendInput` puts the synthetic mouse events on the **system input queue**,
/// where Chromium's input filter accepts them. The trade-off is a brief
/// foreground swap + visible cursor jump (mitigated by saving/restoring the
/// previous foreground HWND and previous cursor position after the click).
///
/// UIAccess constraint: `SetForegroundWindow` is restricted from non-UIAccess
/// processes when not driven by user input. The `cua-driver-uia` worker runs
/// at UIAccess integrity precisely so this restriction is lifted; outside the
/// worker, the foreground swap may silently fail and SendInput land on the
/// wrong window. Callers should funnel Chromium coord clicks through the
/// uia worker (the MCP proxy already prefers the uia pipe over the regular
/// pipe when both are running).
pub fn send_click_synthesized(
    target: u64,
    sx: i32,
    sy: i32,
    count: usize,
    button: &str,
) -> Result<()> {
    send_click_synthesized_mods(target, sx, sy, count, button, &[])
}

/// Like [`send_click_synthesized`] but HOLDS the named modifier keys
/// (cmd/shift/option/ctrl) for the duration of the click: modifier-down via
/// SendInput before the click sequence, modifier-up after. Mirrors the macOS
/// click `modifier` surface. Only this SendInput (foreground / desktop-scope)
/// path can carry modifiers — the background UIA-Invoke and PostMessage paths
/// have no keyboard state to hold them, so a `modifier` passed to a background
/// pixel/element click is necessarily ignored on those rungs.
pub fn send_click_synthesized_mods(
    target: u64,
    sx: i32,
    sy: i32,
    count: usize,
    button: &str,
    modifiers: &[&str],
) -> Result<()> {
    send_click_synthesized_mods_impl(target, sx, sy, count, button, modifiers, false)
}

/// SendInput click for an explicit foreground request. Unlike the historical
/// z-order-assisted path, this activates the target and does not add
/// `WS_EX_NOACTIVATE`, so retained-mode frameworks such as WPF process the
/// system-queue pointer event. The caller owns any later foreground restore.
pub fn send_click_synthesized_active_mods(
    target: u64,
    sx: i32,
    sy: i32,
    count: usize,
    button: &str,
    modifiers: &[&str],
) -> Result<()> {
    send_click_synthesized_mods_impl(target, sx, sy, count, button, modifiers, true)
}

fn send_click_synthesized_mods_impl(
    target: u64,
    sx: i32,
    sy: i32,
    count: usize,
    button: &str,
    modifiers: &[&str],
    activate: bool,
) -> Result<()> {
    let target = HWND(target as *mut _);
    if target.0.is_null() {
        bail!("invalid target hwnd");
    }
    if let Some(msg) = crate::input::post_message_blocked_by_uipi(target.0 as u64) {
        // Same UIPI defense as PostMessage path — SendInput from non-UIAccess
        // would fail just as silently as PostMessage when target is at higher
        // integrity. Surface the diagnostic early.
        bail!(msg);
    }

    let (down_flag, up_flag) = match button {
        "right" => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP),
        "middle" => (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP),
        _ => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP),
    };

    // Convert screen pixel coords to normalized absolute coords spanning the
    // virtual desktop (0..65535 across the union of all monitors). This is
    // what `MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK` expects.
    //
    // Without VIRTUALDESK the coords are relative to the primary monitor only;
    // multi-monitor setups would misroute. Better to always use VIRTUALDESK.
    //
    // Math lives in `crate::virtualdesk` so it can be unit-tested cross-platform
    // (no Win32 runtime required) — see issue #1979 for the negative-offset
    // multi-monitor case the tests there pin down.
    let (vd_x, vd_y) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
        )
    };
    let (vd_w, vd_h) = unsafe {
        (
            GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1),
            GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1),
        )
    };
    let (norm_x, norm_y) =
        crate::virtualdesk::to_virtualdesk_absolute(sx, sy, vd_x, vd_y, vd_w, vd_h);

    let move_input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: norm_x,
                dy: norm_y,
                mouseData: 0,
                dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    let down_input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: 0,
                dwFlags: down_flag,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    let up_input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: 0,
                dwFlags: up_flag,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };

    unsafe {
        // Save previous foreground + cursor position so we can restore.
        let prev_fg = GetForegroundWindow();
        let mut prev_cursor = POINT::default();
        let _ = GetCursorPos(&mut prev_cursor);

        // Bring the target to the top of the VISIBLE z-order so the
        // coordinate-routed SendInput mouse click lands on it — WITHOUT stealing
        // focus. `SetWindowPos(HWND_TOPMOST, SWP_NOACTIVATE)` is **lock-free**:
        // it works from a non-UIAccess process even on a maxed foreground-lock
        // (unlike `SetForegroundWindow`, which the lock denies), and sends no
        // WM_ACTIVATE. `NoActivateGuard` then keeps the click itself from
        // activating the target. This is the macOS-aligned "front → act →
        // restore" for pointer input, done the one Windows way that doesn't
        // need UIAccess — the technique the OG GTK path used. (Keyboard
        // foreground still needs *real* focus; only pointer can be z-routed.)
        // Capture whether the target was ALREADY always-on-top so we don't strip
        // that state on restore — only demote below if WE promoted it.
        let was_topmost = (GetWindowLongPtrW(target, GWL_EXSTYLE) as u32) & WS_EX_TOPMOST.0 != 0;
        if activate && !crate::input::force_foreground_assisted(target).0 {
            let actual = GetForegroundWindow();
            bail!(
                "foreground_unavailable: Windows did not activate exact target HWND {:?} \
                 (actual foreground HWND {:?}); no mouse input was sent",
                target.0,
                actual.0
            );
        }
        let foreground_target = if activate {
            match crate::win32::capture_foreground_target(target.0 as usize as u64) {
                Some(target) => Some(target),
                None => bail!(
                    "foreground_unavailable: exact target HWND {:?} disappeared before mouse \
                     input could be sent",
                    target.0
                ),
            }
        } else {
            None
        };
        let noactivate = (!activate).then(|| crate::input::NoActivateGuard::arm(target));
        if !activate {
            let _ = SetWindowPos(
                target,
                HWND_TOPMOST,
                0,
                0,
                0,
                0,
                SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE,
            );
        }

        // Move the cursor so the OS hover state matches before the click; the
        // MOUSEEVENTF_MOVE input ensures Chromium's input filter sees a
        // coordinated move event.
        let _ = SetCursorPos(sx, sy);

        // Hold modifier keys (ctrl/shift/alt/win) across the click — the macOS
        // `modifier` surface. Pressed via the system input queue so apps that
        // poll GetKeyState (WPF, Chromium) observe the held state, then released
        // after the click loop below.
        let (mod_downs, mod_ups) = crate::input::keyboard::modifier_hold_inputs(modifiers);
        let mut sent_ok = true;
        if !mod_downs.is_empty() {
            let sent = SendInput(&mod_downs, std::mem::size_of::<INPUT>() as i32);
            sent_ok = sent as usize == mod_downs.len();
            sleep(Duration::from_millis(5));
        }

        let count = count.max(1);
        for i in 0..count {
            if !sent_ok {
                break;
            }
            // Only the move record carries absolute coordinates. Button-only
            // records act at the current pointer position; adding ABSOLUTE to
            // them can prevent retained-mode controls from seeing the press.
            let events = [move_input, down_input, up_input];
            let sent = SendInput(&events, std::mem::size_of::<INPUT>() as i32);
            if sent as usize != events.len() {
                sent_ok = false;
                break;
            }
            if i + 1 < count {
                sleep(Duration::from_millis(80));
            }
        }

        // Release any held modifiers (reverse order) before restoring z-order.
        if !mod_ups.is_empty() {
            let released = SendInput(&mod_ups, std::mem::size_of::<INPUT>() as i32);
            if released as usize != mod_ups.len() {
                // A second release attempt is safe and reduces the chance of a
                // partially inserted chord leaving system modifier state held.
                let _ = SendInput(&mod_ups, std::mem::size_of::<INPUT>() as i32);
                sent_ok = false;
            }
        }

        // Let the target process mouse-up before any background-route restore.
        // Retained-mode frameworks establish capture/focus on mouse-down and can
        // lose the click if the real cursor is warped away while those queued
        // messages are still being dispatched.
        sleep(Duration::from_millis(if activate { 120 } else { 40 }));
        if !was_topmost && !activate {
            let _ = SetWindowPos(
                target,
                HWND_NOTOPMOST,
                0,
                0,
                0,
                0,
                SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE,
            );
        }
        if !activate {
            if !prev_fg.0.is_null() && prev_fg != target {
                let _ = SetWindowPos(
                    prev_fg,
                    HWND_TOP,
                    0,
                    0,
                    0,
                    0,
                    SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE,
                );
            }
            let _ = SetCursorPos(prev_cursor.x, prev_cursor.y);
        }
        drop(noactivate);
        if !sent_ok {
            bail!(
                "SendInput inserted fewer modifier or mouse events than expected for the \
                 foreground click."
            );
        }
        if activate {
            let actual = GetForegroundWindow();
            if !crate::win32::foreground_matches_target_or_owned_window(
                foreground_target.expect("foreground target captured before input"),
                actual.0 as usize as u64,
            ) {
                bail!(
                    "foreground_unavailable: exact target HWND {:?} or a verified same-process \
                     post-action window was not foreground after the click \
                     (actual foreground HWND {:?})",
                    target.0,
                    actual.0
                );
            }
        }
    }

    Ok(())
}

/// Press-hold-move-release drag via `SendInput`. Companion to
/// [`send_click_synthesized`] for the `drag` tool's `delivery_mode:"foreground"`
/// path.
///
/// Why a SendInput drag is needed at all: the PostMessage drag path posts
/// `WM_LBUTTONDOWN` + `WM_MOUSEMOVE`s + `WM_LBUTTONUP` to the target HWND.
/// PostMessage does NOT update the per-thread keyboard state that
/// `GetKeyState(VK_LBUTTON)` reads, so frameworks that poll the button-held
/// state during their drag handler (WPF's Thumb.IsDragging logic does this
/// via Mouse.LeftButton, which polls GetKeyState) never observe the button
/// as down and the drag is a no-op. SendInput goes through the system
/// input queue and DOES update GetKeyState, so a WPF Slider thumb actually
/// tracks the drag.
///
/// Same UIAccess constraints as [`send_click_synthesized`] — the
/// `SetForegroundWindow` swap is rejected from non-UIAccess processes
/// when foreground-lock is active; route through `cua-driver-uia.exe`
/// for reliable operation.
pub fn send_drag_synthesized(
    target: u64,
    sx_from: i32,
    sy_from: i32,
    sx_to: i32,
    sy_to: i32,
    duration_ms: u64,
    steps: usize,
    button: &str,
) -> Result<()> {
    let target = HWND(target as *mut _);
    if target.0.is_null() {
        bail!("invalid target hwnd");
    }
    if let Some(msg) = crate::input::post_message_blocked_by_uipi(target.0 as u64) {
        bail!(msg);
    }

    let (down_flag, up_flag) = match button {
        "right" => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP),
        "middle" => (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP),
        _ => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP),
    };

    let (vd_x, vd_y, vd_w, vd_h) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1),
            GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1),
        )
    };
    // Same VIRTUALDESK normalization as `send_click_synthesized`; see
    // `crate::virtualdesk` for the math + the cross-platform unit tests.
    let norm = |sx: i32, sy: i32| -> (i32, i32) {
        crate::virtualdesk::to_virtualdesk_absolute(sx, sy, vd_x, vd_y, vd_w, vd_h)
    };
    let make_input = |dx: i32, dy: i32, flags| INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: 0,
                dwFlags: flags | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };

    let steps = steps.max(1);
    let step_delay_ms = if steps > 1 {
        duration_ms / steps as u64
    } else {
        0
    };

    unsafe {
        let prev_fg = GetForegroundWindow();
        let mut prev_cursor = POINT::default();
        let _ = GetCursorPos(&mut prev_cursor);

        // Lock-free z-order raise (no focus steal) so the coordinate-routed drag
        // lands on the target — same technique as send_click_synthesized.
        // SetForegroundWindow is lock-denied without UIAccess and isn't needed
        // for pointer input; NoActivateGuard keeps the press from activating it.
        let _noact = crate::input::NoActivateGuard::arm(target);
        // Capture whether the target was ALREADY always-on-top so we only demote
        // below if WE promoted it (else we'd strip a legitimate topmost window).
        let was_topmost = (GetWindowLongPtrW(target, GWL_EXSTYLE) as u32) & WS_EX_TOPMOST.0 != 0;
        let _ = SetWindowPos(
            target,
            HWND_TOPMOST,
            0,
            0,
            0,
            0,
            SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE,
        );

        // 1. Move + press at the start of the drag.
        let (nfx, nfy) = norm(sx_from, sy_from);
        let _ = SetCursorPos(sx_from, sy_from);
        let prelude = [
            make_input(nfx, nfy, MOUSEEVENTF_MOVE),
            make_input(nfx, nfy, down_flag),
        ];
        let sent = SendInput(&prelude, std::mem::size_of::<INPUT>() as i32);
        if sent as usize != prelude.len() {
            if !was_topmost {
                let _ = SetWindowPos(
                    target,
                    HWND_NOTOPMOST,
                    0,
                    0,
                    0,
                    0,
                    SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE,
                );
            }
            if !prev_fg.0.is_null() && prev_fg != target {
                let _ = SetWindowPos(
                    prev_fg,
                    HWND_TOP,
                    0,
                    0,
                    0,
                    0,
                    SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE,
                );
            }
            let _ = SetCursorPos(prev_cursor.x, prev_cursor.y);
            bail!(
                "SendInput drag-prelude inserted {sent}/{} events",
                prelude.len()
            );
        }

        // 2. Interpolate the path. SetCursorPos + MOUSEEVENTF_MOVE in lockstep
        //    so both the visible cursor and the system input queue track the
        //    same path — WPF's drag-handler watches GetKeyState during each
        //    move event.
        for i in 1..=steps {
            let t = i as f64 / steps as f64;
            let x = sx_from + ((sx_to - sx_from) as f64 * t).round() as i32;
            let y = sy_from + ((sy_to - sy_from) as f64 * t).round() as i32;
            let (nx, ny) = norm(x, y);
            let _ = SetCursorPos(x, y);
            let mv = [make_input(nx, ny, MOUSEEVENTF_MOVE)];
            let _ = SendInput(&mv, std::mem::size_of::<INPUT>() as i32);
            if step_delay_ms > 0 {
                sleep(Duration::from_millis(step_delay_ms));
            }
        }

        // 3. Release at the end.
        let (ntx, nty) = norm(sx_to, sy_to);
        let release = [make_input(ntx, nty, up_flag)];
        let _ = SendInput(&release, std::mem::size_of::<INPUT>() as i32);

        // Brief settle, then restore z-order (demote target, restack user's
        // window — no activation) and the cursor.
        sleep(Duration::from_millis(40));
        if !was_topmost {
            let _ = SetWindowPos(
                target,
                HWND_NOTOPMOST,
                0,
                0,
                0,
                0,
                SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE,
            );
        }
        if !prev_fg.0.is_null() && prev_fg != target {
            let _ = SetWindowPos(
                prev_fg,
                HWND_TOP,
                0,
                0,
                0,
                0,
                SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE,
            );
        }
        let _ = SetCursorPos(prev_cursor.x, prev_cursor.y);
        drop(_noact);
    }

    Ok(())
}

/// Standard wheel notch delta. A `mouseData` value of `±WHEEL_DELTA` is one
/// detent of the physical mouse wheel.
const WHEEL_DELTA: i32 = 120;

/// Compute the `MOUSEINPUT::mouseData` value for a wheel event of `ticks`
/// detents. Positive ticks = wheel forward/up (vertical) or right (horizontal);
/// negative = down / left. `mouseData` is a `u32` field carrying a signed
/// 32-bit delta, so we compute as `i32` then bit-cast to `u32` (this is what
/// the Win32 docs mean by "the value is a multiple of WHEEL_DELTA").
///
/// Factored out of [`send_wheel_synthesized`] so the sign/magnitude encoding is
/// unit-testable without a live display / `SendInput`.
fn wheel_mouse_data(ticks: i32) -> u32 {
    (WHEEL_DELTA * ticks) as u32
}

/// Synthesize a desktop-scoped wheel event at `(sx, sy)`.
///
/// Wheel routing depends on focus, the inactive-scroll setting and application
/// message handling; cursor placement alone is not proof of target consumption.
/// Window-scoped callers must use `send_wheel_synthesized_to_window` instead.
/// An inserted input remains unverified until the caller observes the view.
pub fn send_wheel_synthesized(sx: i32, sy: i32, ticks: i32, horizontal: bool) -> Result<()> {
    send_wheel_synthesized_checked(sx, sy, ticks, horizontal, || Ok(()))
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum WheelTargetRefusalCode {
    Unavailable,
    Unsupported,
}

impl WheelTargetRefusalCode {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Unavailable => "wheel_target_unavailable",
            Self::Unsupported => "wheel_target_unsupported",
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub(crate) struct WheelTargetRefused {
    pub(crate) code: WheelTargetRefusalCode,
    message: String,
}

impl WheelTargetRefused {
    fn unavailable(message: impl Into<String>) -> Self {
        Self {
            code: WheelTargetRefusalCode::Unavailable,
            message: message.into(),
        }
    }
    fn unsupported(message: impl Into<String>) -> Self {
        Self {
            code: WheelTargetRefusalCode::Unsupported,
            message: message.into(),
        }
    }
}

/// The existing foreground transaction plus an exact wheel-target guard.
/// It never clicks, selects an item, or sets control focus as a side effect.
pub(crate) fn send_wheel_synthesized_to_window(
    hwnd: u64,
    expected_pid: u32,
    sx: i32,
    sy: i32,
    ticks: i32,
    horizontal: bool,
) -> Result<()> {
    let target = HWND(hwnd as *mut _);
    let mut actual_pid = 0;
    let thread = unsafe {
        windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId(
            target,
            Some(&mut actual_pid),
        )
    };
    if thread == 0 || actual_pid != expected_pid {
        return Err(WheelTargetRefused::unavailable(
            "wheel_target_unavailable: target does not belong to expected process; no activation or input was attempted",
        ).into());
    }
    // Existing UWP launch exposes the AFH frame HWND/PID, but hosted UIA
    // focus has an app PID. The resolver proves physical hosting, not a
    // unique cross-process focus authorization; do not widen this guard.
    if crate::win32::resolve_uwp_app_pid(expected_pid, hwnd).is_some() {
        return Err(WheelTargetRefused::unsupported(
            "wheel_target_unsupported: ApplicationFrameHost hosted-content wheel is outside this candidate's verified target boundary; no activation or wheel input was attempted",
        ).into());
    }
    let root = unsafe { GetAncestor(target, GA_ROOT) };
    if root.0.is_null() {
        return Err(WheelTargetRefused::unavailable(
            "wheel_target_unavailable: target root disappeared; no input was sent",
        )
        .into());
    }
    let mut dispatch_attempted = false;
    let result = crate::input::keyboard::with_confirmed_foreground(
        root,
        "wheel delivery",
        || Ok(()),
        || {
            send_wheel_synthesized_checked(sx, sy, ticks, horizontal, || {
                confirm_wheel_target(target, root, expected_pid, sx, sy)?;
                dispatch_attempted = true;
                Ok(())
            })
        },
    );
    if !dispatch_attempted {
        result.map_err(|error| {
            // A provider's IErrorInfo may contain arbitrary control content.
            // Preserve only its HRESULT, not value/title strings.
            let message = if let Some(native) = error.downcast_ref::<windows::core::Error>() {
                format!(
                    "wheel_target_unavailable: native target/focus query failed (HRESULT 0x{:08X})",
                    native.code().0 as u32
                )
            } else {
                format!("{error:#}")
            };
            WheelTargetRefused::unavailable(format!("{message}; no wheel input was sent")).into()
        })
    } else {
        result
    }
}

/// Admit a wheel only when native hover and focus belong to the exact root,
/// and UIA identifies precise control focus, or a proved virtual document
/// fragment with exact native-child ownership. Hover alone is insufficient.
/// A same-process owner/sibling window is deliberately insufficient here.
/// All UIA reads are observational; unavailable/ambiguous providers fail closed.
fn confirm_wheel_target(
    target: HWND,
    root: HWND,
    expected_pid: u32,
    sx: i32,
    sy: i32,
) -> Result<()> {
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
        COINIT_APARTMENTTHREADED,
    };
    use windows::Win32::UI::Accessibility::{
        CUIAutomation, IUIAutomation, IUIAutomationElement, UIA_DataGridControlTypeId,
        UIA_ListControlTypeId, UIA_ScrollPatternId, UIA_TableControlTypeId, UIA_TreeControlTypeId,
    };
    let before_native = read_wheel_native_witness(target, root, expected_pid, sx, sy)?;

    // Scope every COM owner so Release/CoUninitialize (which can RPC/pump
    // messages) finish BEFORE the final native-only witness.
    let virtual_anchor = {
        // Balance only a successful COM initialisation. An existing apartment with
        // another concurrency model may still create the UIA object; never uninit it.
        struct ComInit(bool);
        impl Drop for ComInit {
            fn drop(&mut self) {
                if self.0 {
                    unsafe { CoUninitialize() };
                }
            }
        }
        let _com = ComInit(unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.is_ok());
        let uia: IUIAutomation =
            unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) }
                .context("wheel_focus_unverifiable: UIA unavailable; no input was sent")?;
        let boundary = unsafe { uia.ElementFromHandle(target) }
            .context("wheel_focus_unverifiable: exact UIA target unavailable; no input was sent")?;
        let focused = unsafe { uia.GetFocusedElement() }
            .context("wheel_focus_unverifiable: focused element unavailable; no input was sent")?;
        let walker = unsafe { uia.RawViewWalker() }?;
        let same = |left: &IUIAutomationElement, right: &IUIAutomationElement| -> Result<bool> {
            Ok(unsafe { uia.CompareElements(left, right) }?.as_bool())
        };
        let parent = |element: &IUIAutomationElement| -> Result<IUIAutomationElement> {
            Ok(unsafe { walker.GetParentElement(element) }?)
        };
        let hovered = unsafe { uia.ElementFromPoint(POINT { x: sx, y: sy }) }
            .context("wheel_focus_unverifiable: hovered element unavailable; no input was sent")?;
        if !wheel_focus_is_descendant(&hovered, &boundary, &boundary, same, parent)? {
            bail!("wheel_target_unavailable: hovered UIA element is outside target; no input was sent");
        }
        // A list item can be hovered while its parent list owns focus. A dialog/
        // pane is NOT enough: accepting that broad common ancestor would allow
        // sibling edit focus (the observed Excel failure) to admit the wheel.
        let scope = nearest_wheel_scope(hovered.clone(), &boundary, same, parent, |current| {
            let kind = unsafe { current.CurrentControlType() }?;
            Ok(kind == UIA_ListControlTypeId
                || kind == UIA_TreeControlTypeId
                || kind == UIA_TableControlTypeId
                || kind == UIA_DataGridControlTypeId
                || unsafe { current.GetCurrentPattern(UIA_ScrollPatternId) }.is_ok())
        })?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "wheel_focus_unverifiable: no precise hovered scroll container; no input was sent"
            )
        })?;
        // Virtual hover semantics are admitted only with a proved windowless
        // Document/native-child fragment. A native list/SDM control still needs
        // exact focus inside its hovered scope; no auto click/focus is added.
        let virtual_scope = match crate::uia::cache::read_input_uia_scope(
            &uia,
            &scope,
            &boundary,
            target.0 as usize as u64,
            expected_pid,
            false,
        ) {
            Ok(scope) => Some(scope),
            Err(crate::uia::cache::KeyboardUiaWitnessError::ProcessMismatch) => None,
            Err(error) => return Err(wheel_scope_query_error(error)),
        };
        let hover_provider_pid = if virtual_scope.is_some() {
            Some(crate::uia::cache::read_input_uia_pid(&hovered).map_err(wheel_scope_query_error)?)
        } else {
            None
        };
        let virtual_focus = if let Some(scope) = &virtual_scope {
            let focus = crate::uia::cache::read_input_uia_scope(
                &uia,
                &focused,
                &boundary,
                target.0 as usize as u64,
                expected_pid,
                false,
            )
            .map_err(wheel_scope_query_error)?;
            if hover_provider_pid != Some(focus.primitive.provider_pid)
                || !scope
                    .same_virtual_document(&focus, &uia)
                    .map_err(wheel_scope_query_error)?
            {
                bail!("wheel_focus_mismatch: virtual focus is outside the exact hovered document fragment; no input was sent");
            }
            Some(focus)
        } else {
            if crate::uia::cache::read_input_uia_pid(&focused).map_err(wheel_scope_query_error)?
                != expected_pid
                || !wheel_focus_is_descendant(&focused, &scope, &boundary, same, parent)?
            {
                bail!("wheel_focus_mismatch: focus is outside hovered native scroll container; no input was sent; focus the intended container with a separate standard action and take a fresh observation");
            }
            None
        };

        // Refresh all provider-derived identities/ancestry first. An unchanged
        // HWND can still host a different virtual sibling control or scroll scope.
        let now_boundary = unsafe { uia.ElementFromHandle(target) }?;
        let now_hovered = unsafe { uia.ElementFromPoint(POINT { x: sx, y: sy }) }?;
        let now_focused = unsafe { uia.GetFocusedElement() }?;
        if !same(&boundary, &now_boundary)?
            || !same(&hovered, &now_hovered)?
            || !same(&focused, &now_focused)?
            || !wheel_focus_is_descendant(&now_hovered, &now_boundary, &now_boundary, same, parent)?
        {
            bail!("wheel_target_unavailable: UIA target/hover/focus identity changed during admission; no input was sent");
        }
        let now_scope = nearest_wheel_scope(
            now_hovered.clone(),
            &now_boundary,
            same,
            parent,
            |current| {
                let kind = unsafe { current.CurrentControlType() }?;
                Ok(kind == UIA_ListControlTypeId
                    || kind == UIA_TreeControlTypeId
                    || kind == UIA_TableControlTypeId
                    || kind == UIA_DataGridControlTypeId
                    || unsafe { current.GetCurrentPattern(UIA_ScrollPatternId) }.is_ok())
            },
        )?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "wheel_focus_unverifiable: hovered scroll scope disappeared; no input was sent"
            )
        })?;
        if !same(&scope, &now_scope)? {
            bail!("wheel_focus_mismatch: hovered scroll scope changed; no input was sent");
        }
        if let (Some(previous_scope), Some(previous_focus)) = (&virtual_scope, &virtual_focus) {
            let refreshed_scope = crate::uia::cache::read_input_uia_scope(
                &uia,
                &now_scope,
                &now_boundary,
                target.0 as usize as u64,
                expected_pid,
                false,
            )
            .map_err(wheel_scope_query_error)?;
            let refreshed_focus = crate::uia::cache::read_input_uia_scope(
                &uia,
                &now_focused,
                &now_boundary,
                target.0 as usize as u64,
                expected_pid,
                false,
            )
            .map_err(wheel_scope_query_error)?;
            if !previous_scope
                .same_identity(&refreshed_scope, &uia)
                .map_err(wheel_scope_query_error)?
                || !previous_focus
                    .same_identity(&refreshed_focus, &uia)
                    .map_err(wheel_scope_query_error)?
                || !refreshed_scope
                    .same_virtual_document(&refreshed_focus, &uia)
                    .map_err(wheel_scope_query_error)?
            {
                bail!("wheel_target_unavailable: virtual document/native-anchor ownership changed; no input was sent");
            }
            // Recheck exact focus/hover identities after every ancestry/provider
            // callback; every COM owner is then dropped before native admission.
            let last_hover = unsafe { uia.ElementFromPoint(POINT { x: sx, y: sy }) }?;
            let last_focus = unsafe { uia.GetFocusedElement() }?;
            if !same(&now_hovered, &last_hover)?
                || !same(&now_focused, &last_focus)?
                || Some(
                    crate::uia::cache::read_input_uia_pid(&last_hover)
                        .map_err(wheel_scope_query_error)?,
                ) != hover_provider_pid
                || crate::uia::cache::read_input_uia_pid(&last_focus)
                    .map_err(wheel_scope_query_error)?
                    != refreshed_focus.primitive.provider_pid
            {
                bail!("wheel_target_unavailable: virtual hover/focus changed during admission; no input was sent");
            }
            Some(refreshed_scope.primitive)
        } else {
            if crate::uia::cache::read_input_uia_pid(&now_focused)
                .map_err(wheel_scope_query_error)?
                != expected_pid
                || !wheel_focus_is_descendant(
                    &now_focused,
                    &now_scope,
                    &now_boundary,
                    same,
                    parent,
                )?
            {
                bail!("wheel_focus_mismatch: hovered native scope or focus membership changed; no input was sent");
            }
            None
        }
    };

    // All UIA reads, interface Release and CoUninitialize have completed.
    // No COM owner/drop/provider read lies between this witness and SendInput.
    let final_native = read_wheel_native_witness(target, root, expected_pid, sx, sy)?;
    if !wheel_native_witness_unchanged(&before_native, &final_native) {
        bail!("wheel_target_unavailable: native target/root/hover/focus changed after provider reads; no input was sent");
    }
    if let Some(anchor) = virtual_anchor {
        // Bind membership to the latest native sample, not before_native. A
        // reparented intermediate ancestor may leave leaf HWND/root unchanged.
        // Two native-only passes refresh physical anchor and containment; no
        // COM/provider work or destructor remains between them and dispatch.
        confirm_virtual_wheel_native_relation(anchor, target, &final_native)?;
        let latest = read_wheel_native_witness(target, root, expected_pid, sx, sy)?;
        if !wheel_native_witness_unchanged(&final_native, &latest) {
            bail!("wheel_target_unavailable: virtual native identities changed during final admission; no input was sent");
        }
        confirm_virtual_wheel_native_relation(anchor, target, &latest)?;
    }
    Ok(())
}

fn virtual_wheel_relation_admitted(
    expected: crate::uia::cache::InputUiaScopeWitness,
    actual: crate::uia::cache::InputUiaScopeWitness,
    hover_within_anchor: bool,
    focus_within_anchor: bool,
) -> bool {
    expected == actual
        && expected.ownership == crate::uia::cache::InputUiaOwnership::VirtualDocument
        && expected.own_native_hwnd == 0
        && expected.nearest_native_hwnd != expected.anchor_root
        && hover_within_anchor
        && focus_within_anchor
}
fn confirm_virtual_wheel_native_relation(
    anchor: crate::uia::cache::InputUiaScopeWitness,
    target: HWND,
    latest: &WheelNativeWitness,
) -> Result<()> {
    let refreshed = crate::uia::cache::read_input_native_scope(anchor, target.0 as usize as u64)
        .map_err(wheel_scope_query_error)?;
    let child = HWND(anchor.nearest_native_hwnd as *mut _);
    let belongs = |h: u64| {
        h == anchor.nearest_native_hwnd
            || unsafe { windows::Win32::UI::WindowsAndMessaging::IsChild(child, HWND(h as *mut _)) }
                .as_bool()
    };
    if !virtual_wheel_relation_admitted(
        anchor,
        refreshed,
        belongs(latest.hover),
        belongs(latest.focus),
    ) {
        bail!("wheel_focus_mismatch: latest native hover/focus or ownership is outside proved virtual anchor; no input was sent");
    }
    Ok(())
}

fn wheel_scope_query_error(error: crate::uia::cache::KeyboardUiaWitnessError) -> anyhow::Error {
    use crate::uia::cache::KeyboardUiaWitnessError as Failure;
    match error {
        Failure::Unavailable(Some(hr)) => anyhow::anyhow!(
            "wheel_target_unavailable: virtual ownership query failed (HRESULT 0x{:08X}); no input was sent", hr as u32),
        _ => anyhow::anyhow!("wheel_target_unavailable: precise native or virtual target ownership could not be proved; no input was sent"),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct WheelNativeWitness {
    target: u64,
    target_pid: u32,
    target_thread: u32,
    target_root: u64,
    root: u64,
    root_pid: u32,
    root_thread: u32,
    foreground: u64,
    cursor: (i32, i32),
    hover: u64,
    hover_root: u64,
    hover_in_target: bool,
    focus: u64,
    focus_root: u64,
    focus_in_target: bool,
}

/// Native-only phase. This must run after the last UIA/provider callback.
fn read_wheel_native_witness(
    target: HWND,
    root: HWND,
    expected_pid: u32,
    sx: i32,
    sy: i32,
) -> Result<WheelNativeWitness> {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetGUIThreadInfo, GetWindowThreadProcessId, IsChild, WindowFromPoint, GUITHREADINFO,
    };
    let mut target_pid = 0;
    let target_thread = unsafe { GetWindowThreadProcessId(target, Some(&mut target_pid)) };
    let mut root_pid = 0;
    let root_thread = unsafe { GetWindowThreadProcessId(root, Some(&mut root_pid)) };
    if root_thread == 0 {
        bail!("wheel_target_unavailable: exact root thread disappeared; no input was sent");
    }
    let hover = unsafe { WindowFromPoint(POINT { x: sx, y: sy }) };
    let mut info = GUITHREADINFO {
        cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
        ..Default::default()
    };
    unsafe { GetGUIThreadInfo(root_thread, &mut info) }?;
    let mut cursor = POINT::default();
    unsafe { GetCursorPos(&mut cursor) }?;
    let witness = WheelNativeWitness {
        target: target.0 as u64,
        target_pid,
        target_thread,
        target_root: unsafe { GetAncestor(target, GA_ROOT) }.0 as u64,
        root: root.0 as u64,
        root_pid,
        root_thread,
        foreground: unsafe { GetForegroundWindow() }.0 as u64,
        cursor: (cursor.x, cursor.y),
        hover: hover.0 as u64,
        hover_root: unsafe { GetAncestor(hover, GA_ROOT) }.0 as u64,
        hover_in_target: hover == target || unsafe { IsChild(target, hover) }.as_bool(),
        focus: info.hwndFocus.0 as u64,
        focus_root: unsafe { GetAncestor(info.hwndFocus, GA_ROOT) }.0 as u64,
        focus_in_target: info.hwndFocus == target
            || unsafe { IsChild(target, info.hwndFocus) }.as_bool(),
    };
    if !wheel_native_witness_valid(&witness, expected_pid, (sx, sy)) {
        bail!("wheel_target_unavailable: exact native target/hover/focus containment could not be confirmed; no input was sent");
    }
    Ok(witness)
}

fn wheel_native_witness_valid(
    w: &WheelNativeWitness,
    expected_pid: u32,
    point: (i32, i32),
) -> bool {
    w.target != 0
        && w.target_pid == expected_pid
        && w.target_thread != 0
        && w.root != 0
        && w.root_pid != 0
        && w.root_thread != 0
        && w.target_root == w.root
        && w.foreground == w.root
        && w.cursor == point
        && w.hover != 0
        && w.hover_root == w.root
        && w.hover_in_target
        && w.focus != 0
        && w.focus_root == w.root
        && w.focus_in_target
}

fn wheel_native_witness_unchanged(before: &WheelNativeWitness, after: &WheelNativeWitness) -> bool {
    before == after
}

fn send_wheel_synthesized_checked(
    sx: i32,
    sy: i32,
    ticks: i32,
    horizontal: bool,
    before_send: impl FnOnce() -> Result<()>,
) -> Result<()> {
    let flag = if horizontal {
        MOUSEEVENTF_HWHEEL
    } else {
        MOUSEEVENTF_WHEEL
    };
    let wheel_input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: wheel_mouse_data(ticks),
                dwFlags: flag,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    let mut prev_cursor = POINT::default();
    unsafe { GetCursorPos(&mut prev_cursor) }
        .context("wheel cursor capture failed; no input was sent")?;
    let result = (|| {
        unsafe { SetCursorPos(sx, sy) }
            .context("wheel cursor positioning failed; no input was sent")?;
        let mut placed = POINT::default();
        unsafe { GetCursorPos(&mut placed) }?;
        if placed.x != sx || placed.y != sy {
            bail!("wheel cursor was clipped or moved; no input was sent");
        }
        insert_wheel_after_admission(before_send, || {
            let events = [wheel_input];
            let sent = unsafe { SendInput(&events, std::mem::size_of::<INPUT>() as i32) };
            if sent as usize != events.len() {
                bail!("SendInput inserted {sent}/{} wheel events", events.len());
            }
            Ok(())
        })?;
        sleep(Duration::from_millis(20));
        Ok(())
    })();
    // Redact arbitrary native IErrorInfo before combining two failures.
    let result = result.map_err(|error: anyhow::Error| {
        if let Some(native) = error.downcast_ref::<windows::core::Error>() {
            anyhow::anyhow!(
                "wheel native operation failed (HRESULT 0x{:08X})",
                native.code().0 as u32
            )
        } else {
            error
        }
    });
    let restoration_hresult = unsafe { SetCursorPos(prev_cursor.x, prev_cursor.y) }
        .err()
        .map(|error| error.code().0 as u32);
    combine_wheel_and_restore(result, restoration_hresult)
}

/// Preserve restoration limitations even when admission/dispatch also failed.
/// The primary error is already redacted by the caller; only a numeric native
/// restoration HRESULT is admitted here. Neither operation is retried.
fn combine_wheel_and_restore(result: Result<()>, restoration_hresult: Option<u32>) -> Result<()> {
    match (result, restoration_hresult) {
        (Ok(()), None) => Ok(()),
        (Err(primary), None) => Err(primary),
        (Err(primary), Some(code)) => Err(primary.context(format!(
            "cursor restoration also failed (HRESULT 0x{code:08X}); pointer position may be changed; no automatic retry was performed"
        ))),
        (Ok(()), Some(code)) => bail!(
            "wheel input was inserted but cursor restoration failed (HRESULT 0x{code:08X}); observe the same target before any retry"
        ),
    }
}

// Shared production boundaries: injectable operations allow native unit tests
// to prove that an admission miss cannot cross the SendInput boundary.
fn insert_wheel_after_admission(
    admit: impl FnOnce() -> Result<()>,
    insert: impl FnOnce() -> Result<()>,
) -> Result<()> {
    admit()?;
    insert()
}

fn wheel_focus_is_descendant<T: Clone>(
    element: &T,
    ancestor: &T,
    boundary: &T,
    same: impl Fn(&T, &T) -> Result<bool>,
    parent: impl Fn(&T) -> Result<T>,
) -> Result<bool> {
    let mut current = element.clone();
    for _ in 0..32 {
        if same(&current, ancestor)? {
            return Ok(true);
        }
        if same(&current, boundary)? {
            return Ok(false);
        }
        current = parent(&current)?;
    }
    Ok(false)
}

fn nearest_wheel_scope<T: Clone>(
    mut current: T,
    boundary: &T,
    same: impl Fn(&T, &T) -> Result<bool>,
    parent: impl Fn(&T) -> Result<T>,
    is_scope: impl Fn(&T) -> Result<bool>,
) -> Result<Option<T>> {
    for _ in 0..32 {
        if is_scope(&current)? {
            return Ok(Some(current));
        }
        if same(&current, boundary)? {
            return Ok(None);
        }
        current = parent(&current)?;
    }
    Ok(None)
}

#[cfg(test)]
mod wheel_target_admission_tests {
    use super::{
        combine_wheel_and_restore, insert_wheel_after_admission, nearest_wheel_scope,
        wheel_focus_is_descendant, wheel_native_witness_unchanged, wheel_native_witness_valid,
        WheelNativeWitness, WheelTargetRefused,
    };
    use anyhow::{bail, Result};
    use std::cell::Cell;

    // IDs model a shared-HWND dialog: root=1, list=2, item=3,
    // sibling edit=4. The same production traversal uses UIA identity/parents.
    fn parent(node: &u8) -> Result<u8> {
        match node {
            3 => Ok(2),
            2 | 4 => Ok(1),
            _ => bail!("no readable parent"),
        }
    }
    fn same(a: &u8, b: &u8) -> Result<bool> {
        Ok(a == b)
    }

    #[test]
    fn unsupported_hosted_target_has_distinct_refusal_code() {
        assert_eq!(
            WheelTargetRefused::unsupported("hosted target outside supported boundary")
                .code
                .as_str(),
            "wheel_target_unsupported"
        );
        assert_eq!(
            WheelTargetRefused::unavailable("target changed")
                .code
                .as_str(),
            "wheel_target_unavailable"
        );
    }

    fn witness() -> WheelNativeWitness {
        WheelNativeWitness {
            target: 11,
            target_pid: 100,
            target_thread: 200,
            target_root: 10,
            root: 10,
            root_pid: 100,
            root_thread: 200,
            foreground: 10,
            cursor: (150, 170),
            hover: 12,
            hover_root: 10,
            hover_in_target: true,
            focus: 13,
            focus_root: 10,
            focus_in_target: true,
        }
    }

    #[test]
    fn changed_native_child_or_reparent_after_provider_reads_cannot_insert() {
        let before = witness();
        assert!(wheel_native_witness_valid(&before, 100, (150, 170)));
        let calls = Cell::new(0);
        let mut changed_hover = before;
        changed_hover.hover = 14; // Different child, still within the same root.
        let mut reparented_hover = before;
        reparented_hover.hover_in_target = false;
        let mut reparented_target = before;
        reparented_target.target_root = 99;
        let mut reparented_focus = before;
        reparented_focus.focus_root = 99;
        let mut replaced_process = before;
        replaced_process.target_pid = 101;
        for after in [
            changed_hover,
            reparented_hover,
            reparented_target,
            reparented_focus,
            replaced_process,
        ] {
            let result = insert_wheel_after_admission(
                || {
                    if !wheel_native_witness_valid(&after, 100, (150, 170))
                        || !wheel_native_witness_unchanged(&before, &after)
                    {
                        bail!("native witness changed after provider phase");
                    }
                    Ok(())
                },
                || {
                    calls.set(calls.get() + 1);
                    Ok(())
                },
            );
            assert!(result.is_err());
            assert_eq!(calls.get(), 0);
        }
    }

    #[test]
    fn native_focus_cursor_or_owner_drift_fails_final_witness() {
        let before = witness();
        let mut focus = before;
        focus.focus = 14;
        let mut cursor = before;
        cursor.cursor = (1, 1);
        let mut root_owner = before;
        root_owner.root_pid = 101;
        for after in [focus, cursor, root_owner] {
            assert!(!wheel_native_witness_unchanged(&before, &after));
        }
        assert!(!wheel_native_witness_valid(&cursor, 100, (150, 170)));
        assert!(wheel_native_witness_unchanged(&before, &before));
    }

    #[test]
    fn admission_and_restoration_failures_are_both_preserved() {
        let error = combine_wheel_and_restore(
            Err(anyhow::anyhow!("focus mismatch; no wheel inserted")),
            Some(0x80070005),
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("focus mismatch; no wheel inserted"));
        assert!(message.contains("cursor restoration also failed (HRESULT 0x80070005)"));
    }

    #[test]
    fn dispatch_and_restoration_failures_are_both_preserved() {
        let error = combine_wheel_and_restore(
            Err(anyhow::anyhow!("SendInput inserted 0/1")),
            Some(0x80070005),
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("SendInput inserted 0/1"));
        assert!(message.contains("cursor restoration also failed (HRESULT 0x80070005)"));
        assert!(combine_wheel_and_restore(Ok(()), None).is_ok());
        let after_input = combine_wheel_and_restore(Ok(()), Some(0x80070005))
            .unwrap_err()
            .to_string();
        assert!(after_input.contains("wheel input was inserted"));
    }

    #[test]
    fn focused_list_descendant_passes_sibling_edit_does_not() {
        let scope = nearest_wheel_scope(3, &1, same, parent, |node| Ok(*node == 2))
            .unwrap()
            .unwrap();
        assert_eq!(scope, 2);
        assert!(wheel_focus_is_descendant(&3, &scope, &1, same, parent).unwrap());
        assert!(wheel_focus_is_descendant(&2, &scope, &1, same, parent).unwrap());
        assert!(!wheel_focus_is_descendant(&4, &scope, &1, same, parent).unwrap());
    }

    #[test]
    fn dialog_boundary_does_not_expand_to_unrelated_scroll_scope() {
        assert!(
            nearest_wheel_scope(4, &1, same, parent, |node| Ok(*node == 9))
                .unwrap()
                .is_none()
        );
        assert!(
            nearest_wheel_scope(3, &1, same, |_| bail!("provider failed"), |_| Ok(false)).is_err()
        );
        assert!(wheel_focus_is_descendant(&3, &2, &1, same, |_| bail!("provider failed")).is_err());
    }

    #[test]
    fn refusal_cannot_insert_input_and_admission_inserts_exactly_once() {
        let calls = Cell::new(0);
        let rejected = insert_wheel_after_admission(
            || bail!("focus mismatch"),
            || {
                calls.set(calls.get() + 1);
                Ok(())
            },
        );
        assert!(rejected.is_err());
        assert_eq!(calls.get(), 0);
        insert_wheel_after_admission(
            || Ok(()),
            || {
                calls.set(calls.get() + 1);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn cyclic_or_overdeep_provider_tree_fails_closed() {
        assert!(!wheel_focus_is_descendant(&3, &2, &1, same, |node| Ok(*node)).unwrap());
        assert!(
            nearest_wheel_scope(3, &1, same, |node| Ok(*node), |_| Ok(false))
                .unwrap()
                .is_none()
        );
    }
}

/// Return the current foreground window for desktop-scope keyboard and drag
/// operations, which intentionally target whatever the user can currently see.
pub fn foreground_window() -> Result<u64> {
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.0.is_null() {
        bail!("no foreground window is available");
    }
    Ok(hwnd.0 as u64)
}

/// Move the real OS pointer to a desktop coordinate.
pub fn move_cursor_desktop(x: i32, y: i32) -> Result<()> {
    unsafe { SetCursorPos(x, y) }
        .map_err(|error| anyhow::anyhow!("SetCursorPos({x}, {y}) failed: {error}"))
}

/// Classify a `WM_NCHITTEST` result into the non-client regions where a
/// posted-message drag can NEVER work: the OS handles caption and border
/// drags with its interactive move/resize modal loop
/// (`WM_NCLBUTTONDOWN` → `DefWindowProc` → `WM_SYSCOMMAND SC_MOVE/SC_SIZE`),
/// which only real pointer input off the system input queue can drive.
/// Posted `WM_MOUSEMOVE` streams are simply discarded, so background
/// delivery must refuse with `background_unavailable` instead of reporting
/// a success that moved nothing.
pub fn classify_nc_move_resize_hit(hit: isize) -> Option<&'static str> {
    // Constant values per winuser.h; kept as literals because the windows
    // crate exposes hit-test codes as u32 while SendMessage returns LRESULT.
    match hit {
        2 => Some("caption / title bar"), // HTCAPTION
        4 => Some("size box"),            // HTGROWBOX / HTSIZE
        10..=17 => Some("resize border"), // HTLEFT..HTBOTTOMRIGHT
        _ => None,
    }
}

/// Screen-coordinate `WM_NCHITTEST` against the drag target's top-level
/// window. Returns the human-readable region name when the point falls on a
/// caption / resize border (the regions a posted drag cannot actuate), else
/// `None`. Uses `SendMessageTimeoutW(SMTO_ABORTIFHUNG)` so a hung target
/// cannot wedge the daemon; timeouts fail open (`None`) — the posted drag
/// then proceeds exactly as before this check existed.
pub fn non_client_move_resize_hit(root: u64, sx: i32, sy: i32) -> Option<&'static str> {
    use windows::Win32::UI::WindowsAndMessaging::{
        SendMessageTimeoutW, SMTO_ABORTIFHUNG, WM_NCHITTEST,
    };
    if root == 0 {
        return None;
    }
    let hwnd = HWND(root as *mut _);
    // Hit-test the top-level frame: the caption belongs to the root window
    // even when the caller resolved a child HWND as the drag target.
    let top = unsafe { GetAncestor(hwnd, GA_ROOT) };
    let top = if top.0.is_null() { hwnd } else { top };
    // WM_NCHITTEST takes SCREEN coordinates packed in LPARAM (signed 16-bit
    // each — correct for any virtual-desktop position within ±32767).
    let lp = LPARAM((((sy as i16 as u16 as u32) << 16) | (sx as i16 as u16 as u32)) as isize);
    let mut result: usize = 0;
    let ok = unsafe {
        SendMessageTimeoutW(
            top,
            WM_NCHITTEST,
            WPARAM(0),
            lp,
            SMTO_ABORTIFHUNG,
            200, // ms — a live window answers this in microseconds
            Some(&mut result),
        )
    };
    if ok.0 == 0 {
        return None; // timed out / hung target: fail open, post as before
    }
    classify_nc_move_resize_hit(result as isize)
}

#[cfg(test)]
mod nc_hit_tests {
    use super::classify_nc_move_resize_hit;

    #[test]
    fn caption_and_borders_refuse_posted_drags_client_area_does_not() {
        assert_eq!(
            classify_nc_move_resize_hit(2), // HTCAPTION
            Some("caption / title bar")
        );
        assert_eq!(classify_nc_move_resize_hit(4), Some("size box")); // HTGROWBOX
        for border in 10..=17 {
            // HTLEFT..HTBOTTOMRIGHT
            assert_eq!(classify_nc_move_resize_hit(border), Some("resize border"));
        }
        assert_eq!(classify_nc_move_resize_hit(1), None); // HTCLIENT
        assert_eq!(classify_nc_move_resize_hit(0), None); // HTNOWHERE
        assert_eq!(classify_nc_move_resize_hit(3), None); // HTSYSMENU (click, not drag-move)
        assert_eq!(classify_nc_move_resize_hit(-1), None); // HTERROR
        assert_eq!(classify_nc_move_resize_hit(18), None); // HTBORDER (non-resizable edge)
    }
}

#[cfg(test)]
mod wheel_tests {
    use super::{posted_press_message, wheel_mouse_data, WHEEL_DELTA};
    use windows::Win32::UI::WindowsAndMessaging::{WM_LBUTTONDBLCLK, WM_LBUTTONDOWN};

    #[test]
    fn posted_double_click_uses_the_win32_double_click_message() {
        assert_eq!(
            posted_press_message(WM_LBUTTONDOWN, WM_LBUTTONDBLCLK, 0, true),
            WM_LBUTTONDOWN
        );
        assert_eq!(
            posted_press_message(WM_LBUTTONDOWN, WM_LBUTTONDBLCLK, 1, true),
            WM_LBUTTONDBLCLK
        );
        assert_eq!(
            posted_press_message(WM_LBUTTONDOWN, WM_LBUTTONDBLCLK, 2, true),
            WM_LBUTTONDOWN
        );
        assert_eq!(
            posted_press_message(WM_LBUTTONDOWN, WM_LBUTTONDBLCLK, 1, false),
            WM_LBUTTONDOWN
        );
    }

    #[test]
    fn wheel_data_up_is_positive_one_notch() {
        // +1 tick (up / right) → +WHEEL_DELTA, bit-cast to u32.
        assert_eq!(wheel_mouse_data(1), WHEEL_DELTA as u32);
        assert_eq!(wheel_mouse_data(1), 120u32);
    }

    #[test]
    fn wheel_data_down_is_negative_one_notch() {
        // -1 tick (down / left) → -WHEEL_DELTA, bit-cast: 0xFFFFFF88.
        assert_eq!(wheel_mouse_data(-1), (-WHEEL_DELTA) as u32);
        assert_eq!(wheel_mouse_data(-1), 0xFFFF_FF88);
    }

    #[test]
    fn wheel_data_scales_with_ticks() {
        assert_eq!(wheel_mouse_data(3), (3 * WHEEL_DELTA) as u32);
        assert_eq!(wheel_mouse_data(3), 360u32);
        assert_eq!(wheel_mouse_data(-3) as i32, -360);
    }

    #[test]
    fn wheel_data_zero_is_zero() {
        assert_eq!(wheel_mouse_data(0), 0);
    }
}

#[cfg(test)]
mod virtual_wheel_ownership_tests {
    use super::*;
    use crate::uia::cache::{InputUiaOwnership, InputUiaScopeWitness};
    fn anchor() -> InputUiaScopeWitness {
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
    #[test]
    fn proved_virtual_document_allows_hover_scope_without_keyboard_focus_in_scroll_control() {
        assert!(virtual_wheel_relation_admitted(
            anchor(),
            anchor(),
            true,
            true
        ));
    }
    #[test]
    fn same_leaf_hwnd_and_root_but_intermediate_focus_ancestor_reparent_refuses() {
        assert!(!virtual_wheel_relation_admitted(
            anchor(),
            anchor(),
            true,
            false
        ));
    }
    #[test]
    fn same_leaf_hwnd_and_root_but_hover_ancestor_reparent_refuses() {
        assert!(!virtual_wheel_relation_admitted(
            anchor(),
            anchor(),
            false,
            true
        ));
    }
    #[test]
    fn native_anchor_parent_drift_refuses() {
        let mut actual = anchor();
        actual.anchor_parent = 99;
        assert!(!virtual_wheel_relation_admitted(
            anchor(),
            actual,
            true,
            true
        ));
    }
    #[test]
    fn wrong_root_or_reused_native_anchor_owner_refuses() {
        let mut actual = anchor();
        actual.anchor_root = 20;
        assert!(!virtual_wheel_relation_admitted(
            anchor(),
            actual,
            true,
            true
        ));
        actual = anchor();
        actual.anchor_pid = 44;
        assert!(!virtual_wheel_relation_admitted(
            anchor(),
            actual,
            true,
            true
        ));
    }
    #[test]
    fn native_sdm_proof_cannot_enter_virtual_hover_relaxation() {
        let mut direct = anchor();
        direct.ownership = InputUiaOwnership::DirectProcess;
        assert!(!virtual_wheel_relation_admitted(direct, direct, true, true));
    }
}
