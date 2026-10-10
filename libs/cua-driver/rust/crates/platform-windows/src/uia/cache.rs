use super::UiaNode;
use cua_driver_core::element_cache::{ElementCacheCore, SnapshotPayload};
use windows::core::Interface;
use windows::Win32::UI::Accessibility::{IAccessible, IUIAutomationElement};

pub type ElementCache = ElementCacheCore<CachedSnapshot>;

#[derive(Debug)]
pub struct RetainedElement {
    ptr: usize,
    pub kind: SnapshotKind,
    pub center: (i32, i32),
    pub rect: Option<(i32, i32, i32, i32)>,
    pub msaa_role: Option<i32>,
}

impl RetainedElement {
    pub fn as_ptr(&self) -> usize {
        self.ptr
    }

    pub fn is_uia(&self) -> bool {
        self.kind == SnapshotKind::Uia
    }

    pub fn focus_element(&self) -> anyhow::Result<()> {
        if !self.is_uia() {
            anyhow::bail!("element is an MSAA element, not a UIA element");
        }
        let element = unsafe { IUIAutomationElement::from_raw(self.ptr as *mut _) };
        let result = unsafe { element.SetFocus() };
        std::mem::forget(element);
        result.map_err(|e| anyhow::anyhow!("UIA SetFocus failed: {e}"))
    }

    pub fn element_has_keyboard_focus(&self) -> Option<bool> {
        if !self.is_uia() {
            return None;
        }
        let element = unsafe { IUIAutomationElement::from_raw(self.ptr as *mut _) };
        let focused = unsafe { element.CurrentHasKeyboardFocus() }
            .ok()
            .map(|value| value.as_bool());
        std::mem::forget(element);
        focused
    }
}

/// Primitive facts only: this type owns no COM objects. The caller must take
/// its final native focus sample only after this method has returned.
#[derive(Clone, Copy, Debug)]
pub(crate) struct KeyboardUiaFocusWitness {
    pub has_focus: bool,
    pub focused_equals_target: bool,
    pub process_matches: bool,
    pub own_native_hwnd: u64,
    pub nearest_native_hwnd: u64,
    pub value_changed_on_focus: Option<bool>,
    pub scope: Option<InputUiaScopeWitness>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum KeyboardUiaWitnessError {
    Unavailable(Option<i32>),
    FocusMismatch,
    ProcessMismatch,
    BoundaryMismatch,
}

/// Validate the signed UIA property before converting to a native PID type.
/// Negative provider values are not unsigned process identities.
fn checked_input_uia_pid(raw: i32) -> Option<u32> {
    if raw <= 0 {
        None
    } else {
        Some(raw as u32)
    }
}

/// Preserve native provider failures; an invalid numeric property is the existing
/// process-ownership refusal, with no provider text or numeric PID serialized.
pub(crate) fn read_input_uia_pid(
    element: &IUIAutomationElement,
) -> Result<u32, KeyboardUiaWitnessError> {
    let raw = unsafe { element.CurrentProcessId() }
        .map_err(|e| KeyboardUiaWitnessError::Unavailable(Some(e.code().0)))?;
    checked_input_uia_pid(raw).ok_or(KeyboardUiaWitnessError::ProcessMismatch)
}

/// Private proof path. This is not serialized: the existing boolean wire means
/// process ownership within the admitted target boundary, not numeric leaf PID
/// equality when this exact virtual-document relation is proved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InputUiaOwnership {
    DirectProcess,
    VirtualDocument,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct InputUiaScopeWitness {
    pub own_native_hwnd: u64,
    pub nearest_native_hwnd: u64,
    pub provider_pid: u32,
    pub anchor_pid: u32,
    pub anchor_thread: u32,
    pub anchor_parent: u64,
    pub anchor_root: u64,
    pub ownership: InputUiaOwnership,
}

/// Native-only anchor refresh; the caller runs this after all COM teardown.
/// The returned Copy value cannot make provider calls or release interfaces.
pub(crate) fn read_input_native_scope(
    mut scope: InputUiaScopeWitness,
    root: u64,
) -> Result<InputUiaScopeWitness, KeyboardUiaWitnessError> {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        GetAncestor, GetParent, GetWindowThreadProcessId, IsChild, IsWindow, GA_ROOT,
    };
    let h = HWND(scope.nearest_native_hwnd as *mut _);
    let admitted = HWND(root as *mut _);
    if !unsafe { IsWindow(h) }.as_bool()
        || !(h == admitted || unsafe { IsChild(admitted, h) }.as_bool())
        || unsafe { GetAncestor(h, GA_ROOT) }.0 as usize as u64 != root
    {
        return Err(KeyboardUiaWitnessError::BoundaryMismatch);
    }
    let mut pid = 0;
    let thread = unsafe { GetWindowThreadProcessId(h, Some(&mut pid)) };
    if pid == 0 || thread == 0 {
        return Err(KeyboardUiaWitnessError::BoundaryMismatch);
    }
    scope.anchor_pid = pid;
    scope.anchor_thread = thread;
    scope.anchor_root = root;
    scope.anchor_parent = match unsafe { GetParent(h) } {
        Ok(p) => p.0 as usize as u64,
        Err(_) if scope.nearest_native_hwnd == root => 0,
        Err(_) => return Err(KeyboardUiaWitnessError::BoundaryMismatch),
    };
    Ok(scope)
}

/// Owned COM facts stay inside the caller's provider phase. A document alone,
/// a same-PID window or a provider's claimed HWND never authorizes virtual input.
pub(crate) struct InputUiaScope {
    pub primitive: InputUiaScopeWitness,
    document: Option<IUIAutomationElement>,
    anchor: IUIAutomationElement,
}
impl InputUiaScope {
    pub(crate) fn same_identity(
        &self,
        other: &Self,
        uia: &windows::Win32::UI::Accessibility::IUIAutomation,
    ) -> Result<bool, KeyboardUiaWitnessError> {
        use KeyboardUiaWitnessError as Failure;
        let same = |a: &IUIAutomationElement, b: &IUIAutomationElement| unsafe {
            uia.CompareElements(a, b)
                .map(|v| v.as_bool())
                .map_err(|e| Failure::Unavailable(Some(e.code().0)))
        };
        if self.primitive != other.primitive || !same(&self.anchor, &other.anchor)? {
            return Ok(false);
        }
        match (&self.document, &other.document) {
            (Some(a), Some(b)) => same(a, b),
            (None, None) => Ok(true),
            _ => Ok(false),
        }
    }
    pub(crate) fn same_virtual_document(
        &self,
        other: &Self,
        uia: &windows::Win32::UI::Accessibility::IUIAutomation,
    ) -> Result<bool, KeyboardUiaWitnessError> {
        if self.primitive.ownership != InputUiaOwnership::VirtualDocument
            || other.primitive.ownership != InputUiaOwnership::VirtualDocument
            || self.primitive.provider_pid == 0
            || self.primitive.provider_pid != other.primitive.provider_pid
            || self.primitive.nearest_native_hwnd != other.primitive.nearest_native_hwnd
            || self.primitive.anchor_pid != other.primitive.anchor_pid
            || self.primitive.anchor_thread != other.primitive.anchor_thread
            || self.primitive.anchor_parent != other.primitive.anchor_parent
            || self.primitive.anchor_root != other.primitive.anchor_root
        {
            return Ok(false);
        }
        match (&self.document, &other.document) {
            (Some(a), Some(b)) => unsafe {
                let same_anchor = uia
                    .CompareElements(&self.anchor, &other.anchor)
                    .map_err(|e| KeyboardUiaWitnessError::Unavailable(Some(e.code().0)))?
                    .as_bool();
                Ok(same_anchor
                    && uia
                        .CompareElements(a, b)
                        .map_err(|e| KeyboardUiaWitnessError::Unavailable(Some(e.code().0)))?
                        .as_bool())
            },
            _ => Ok(false),
        }
    }
}

/// Primitive policy shared by production provider admission and unit tests.
/// Virtual ownership is deliberately narrower than ordinary raw-tree ancestry.
#[derive(Clone, Copy, Debug)]
struct InputOwnershipFacts {
    direct_process: bool,
    windowless: bool,
    document_before_anchor: bool,
    exact_boundary_reached: bool,
    non_root_native_child: bool,
    anchor_element_from_handle_equal: bool,
    anchor_provider_matches_native_owner: bool,
    provider_pid_nonzero: bool,
}
fn classify_input_ownership(f: InputOwnershipFacts) -> Option<InputUiaOwnership> {
    if !f.exact_boundary_reached || !f.provider_pid_nonzero {
        return None;
    }
    if f.windowless
        && f.document_before_anchor
        && f.non_root_native_child
        && f.anchor_element_from_handle_equal
        && f.anchor_provider_matches_native_owner
    {
        Some(InputUiaOwnership::VirtualDocument)
    } else if f.direct_process {
        Some(InputUiaOwnership::DirectProcess)
    } else {
        None
    }
}

/// Read complete <=64 raw ancestry to the exact already-admitted frame. The
/// virtual path binds a real Document fragment to a non-root native child via
/// fresh ElementFromHandle identity, native ancestry and provider/native owner
/// equality. No process-family, class-name, owner-window or arbitrary PID rule.
pub(crate) fn read_input_uia_scope(
    uia: &windows::Win32::UI::Accessibility::IUIAutomation,
    target: &IUIAutomationElement,
    boundary: &IUIAutomationElement,
    hwnd: u64,
    expected_pid: u32,
    allow_direct: bool,
) -> Result<InputUiaScope, KeyboardUiaWitnessError> {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::Accessibility::UIA_DocumentControlTypeId;
    use windows::Win32::UI::WindowsAndMessaging::{
        GetAncestor, GetParent, GetWindowThreadProcessId, IsChild, IsWindow, GA_ROOT,
    };
    use KeyboardUiaWitnessError as Failure;
    let provider_error = |e: windows::core::Error| Failure::Unavailable(Some(e.code().0));
    let same = |a: &IUIAutomationElement, b: &IUIAutomationElement| unsafe {
        uia.CompareElements(a, b)
            .map(|v| v.as_bool())
            .map_err(provider_error)
    };
    if read_input_uia_pid(boundary)? != expected_pid
        || unsafe { boundary.CurrentNativeWindowHandle() }
            .map_err(provider_error)?
            .0 as usize as u64
            != hwnd
    {
        return Err(Failure::BoundaryMismatch);
    }
    let provider_pid = read_input_uia_pid(target)?;
    let own_native_hwnd = unsafe { target.CurrentNativeWindowHandle() }
        .map_err(provider_error)?
        .0 as usize as u64;
    let walker = unsafe { uia.RawViewWalker() }.map_err(provider_error)?;
    let mut current = target.clone();
    let mut document = None;
    let mut anchor = None;
    let mut reached = false;
    for depth in 0..64 {
        let native = unsafe { current.CurrentNativeWindowHandle() }
            .map_err(provider_error)?
            .0 as usize as u64;
        if depth == 0 && native != own_native_hwnd {
            return Err(Failure::BoundaryMismatch);
        }
        // A document encountered only above an earlier native control does not
        // turn that control into a virtual input recipient.
        if anchor.is_none()
            && document.is_none()
            && unsafe { current.CurrentControlType() }.map_err(provider_error)?
                == UIA_DocumentControlTypeId
        {
            document = Some(current.clone());
        }
        if anchor.is_none() && native != 0 {
            anchor = Some((native, current.clone()));
        }
        if same(&current, boundary)? {
            reached = true;
            break;
        }
        current = unsafe { walker.GetParentElement(&current) }.map_err(provider_error)?;
    }
    let (nearest_native_hwnd, anchor) = anchor.ok_or(Failure::BoundaryMismatch)?;
    let native = HWND(nearest_native_hwnd as *mut _);
    let admitted = HWND(hwnd as *mut _);
    let mut anchor_pid = 0;
    let anchor_thread = unsafe { GetWindowThreadProcessId(native, Some(&mut anchor_pid)) };
    let anchor_root = unsafe { GetAncestor(native, GA_ROOT) }.0 as usize as u64;
    if !reached
        || !unsafe { IsWindow(native) }.as_bool()
        || anchor_thread == 0
        || anchor_root != hwnd
        || !(native == admitted || unsafe { IsChild(admitted, native) }.as_bool())
    {
        return Err(Failure::BoundaryMismatch);
    }
    let anchor_parent = match unsafe { GetParent(native) } {
        Ok(p) => p.0 as usize as u64,
        Err(_) if nearest_native_hwnd == hwnd => 0,
        Err(_) => return Err(Failure::BoundaryMismatch),
    };
    let direct_process = allow_direct && provider_pid == expected_pid && anchor_pid == expected_pid;
    let mut anchor_element_from_handle_equal = false;
    let mut anchor_provider_matches_native_owner = false;
    if !direct_process
        || (own_native_hwnd == 0 && document.is_some() && nearest_native_hwnd != hwnd)
    {
        let from_handle = unsafe { uia.ElementFromHandle(native) }.map_err(provider_error)?;
        anchor_element_from_handle_equal = same(&anchor, &from_handle)?;
        anchor_provider_matches_native_owner = read_input_uia_pid(&anchor)? == anchor_pid
            && read_input_uia_pid(&from_handle)? == anchor_pid;
    }
    let ownership = classify_input_ownership(InputOwnershipFacts {
        direct_process,
        windowless: own_native_hwnd == 0,
        document_before_anchor: document.is_some(),
        exact_boundary_reached: reached,
        non_root_native_child: nearest_native_hwnd != hwnd,
        anchor_element_from_handle_equal,
        anchor_provider_matches_native_owner,
        provider_pid_nonzero: provider_pid != 0,
    })
    .ok_or(Failure::ProcessMismatch)?;
    Ok(InputUiaScope {
        primitive: InputUiaScopeWitness {
            own_native_hwnd,
            nearest_native_hwnd,
            provider_pid,
            anchor_pid,
            anchor_thread,
            anchor_parent,
            anchor_root,
            ownership,
        },
        document: if ownership == InputUiaOwnership::VirtualDocument {
            document
        } else {
            None
        },
        anchor,
    })
}

impl RetainedElement {
    /// Compare the admitted object to system focus, not only the provider's
    /// HasKeyboardFocus property. Names and runtime IDs are never read; an
    /// optional ValuePattern equality observation stays private and boolean-only.
    /// All COM interfaces and the apartment guard drop before returning.
    pub(crate) fn keyboard_focus_witness(
        &self,
        hwnd: u64,
        expected_pid: u32,
        value_before_focus: Option<&str>,
    ) -> Result<KeyboardUiaFocusWitness, KeyboardUiaWitnessError> {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::System::Com::{
            CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
            COINIT_APARTMENTTHREADED,
        };
        use windows::Win32::UI::Accessibility::{CUIAutomation, IUIAutomation};
        use KeyboardUiaWitnessError as Failure;
        if !self.is_uia() || self.ptr == 0 {
            return Err(Failure::Unavailable(None));
        }
        let primitive = {
            struct ComInit(bool);
            impl Drop for ComInit {
                fn drop(&mut self) {
                    if self.0 {
                        unsafe { CoUninitialize() };
                    }
                }
            }
            let init = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
            // An already initialized apartment is usable, but must not be
            // uninitialized by us. Other initialization failures fail closed.
            if init.is_err() && init.0 != -2147417850 {
                return Err(Failure::Unavailable(Some(init.0)));
            }
            let _com = ComInit(init.is_ok());
            let provider_error =
                |error: windows::core::Error| Failure::Unavailable(Some(error.code().0));
            let uia: IUIAutomation =
                unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) }
                    .map_err(provider_error)?;
            let boundary =
                unsafe { uia.ElementFromHandle(HWND(hwnd as *mut _)) }.map_err(provider_error)?;
            // Borrow the cache's reference; no Release on a borrowed pointer,
            // including any early provider error below.
            let target = std::mem::ManuallyDrop::new(unsafe {
                IUIAutomationElement::from_raw(self.ptr as *mut _)
            });
            let focused = unsafe { uia.GetFocusedElement() }.map_err(provider_error)?;
            let has_focus = unsafe { target.CurrentHasKeyboardFocus() }
                .map_err(provider_error)?
                .as_bool();
            let focused_equals_target = unsafe { uia.CompareElements(&*target, &focused) }
                .map_err(provider_error)?
                .as_bool();
            let target_pid = read_input_uia_pid(&target)?;
            let focused_pid = read_input_uia_pid(&focused)?;
            if !has_focus || !focused_equals_target || target_pid == 0 || target_pid != focused_pid
            {
                return Ok(KeyboardUiaFocusWitness {
                    has_focus,
                    focused_equals_target,
                    process_matches: target_pid != 0
                        && target_pid == focused_pid
                        && target_pid == expected_pid,
                    own_native_hwnd: 0,
                    nearest_native_hwnd: 0,
                    value_changed_on_focus: None,
                    scope: None,
                });
            }
            let scope_before =
                read_input_uia_scope(&uia, &target, &boundary, hwnd, expected_pid, true)?;
            let value_changed_on_focus = value_before_focus
                .and_then(|before| retained_value(&target).map(|after| before != after));
            // Optional value callbacks can reparent the same focused object or
            // replace its native handle without changing focus/PID. Refresh exact
            // window identity and the complete native/UIA scope after that read.
            let final_boundary =
                unsafe { uia.ElementFromHandle(HWND(hwnd as *mut _)) }.map_err(provider_error)?;
            if !unsafe { uia.CompareElements(&boundary, &final_boundary) }
                .map_err(provider_error)?
                .as_bool()
            {
                return Err(Failure::BoundaryMismatch);
            }
            let scope_after =
                read_input_uia_scope(&uia, &target, &final_boundary, hwnd, expected_pid, true)?;
            if !scope_before.same_identity(&scope_after, &uia)? {
                return Err(Failure::BoundaryMismatch);
            }
            let primitive_scope = scope_after.primitive;
            let own_native_hwnd = primitive_scope.own_native_hwnd;
            let nearest_native_hwnd = primitive_scope.nearest_native_hwnd;
            // Ancestor/value calls can also change semantic focus while sharing
            // a HWND. Refresh exact focus and both process properties last.
            let final_focused = unsafe { uia.GetFocusedElement() }.map_err(provider_error)?;
            let final_has_focus = unsafe { target.CurrentHasKeyboardFocus() }
                .map_err(provider_error)?
                .as_bool();
            let final_same = unsafe { uia.CompareElements(&*target, &final_focused) }
                .map_err(provider_error)?
                .as_bool();
            let final_target_pid = read_input_uia_pid(&target)?;
            let final_focused_pid = read_input_uia_pid(&final_focused)?;
            // Wire true is the proved boundary ownership policy, including the
            // narrowly admitted virtual fragment. It is not a numeric-PID claim.
            let process_matches = target_pid == final_target_pid
                && target_pid == primitive_scope.provider_pid
                && focused_pid == final_focused_pid
                && final_target_pid != 0
                && final_target_pid == final_focused_pid;
            KeyboardUiaFocusWitness {
                has_focus: has_focus && final_has_focus,
                focused_equals_target: focused_equals_target && final_same,
                process_matches,
                own_native_hwnd,
                nearest_native_hwnd,
                value_changed_on_focus,
                scope: Some(primitive_scope),
            }
        };
        // Includes interface Release and CoUninitialize; returning only this
        // Copy value cannot perform provider work after the final Win32 read.
        Ok(primitive)
    }
}

// Private optional equality control. Returned Strings never enter telemetry,
// logs or errors, and all provider owners are released in the caller's scope.
fn retained_value(target: &IUIAutomationElement) -> Option<String> {
    use windows::Win32::UI::Accessibility::{IUIAutomationValuePattern, UIA_ValuePatternId};
    let pattern = unsafe { target.GetCurrentPattern(UIA_ValuePatternId) }.ok()?;
    let value: IUIAutomationValuePattern = pattern.cast().ok()?;
    Some(unsafe { value.CurrentValue() }.ok()?.to_string())
}
impl RetainedElement {
    pub(crate) fn keyboard_value_before_focus(&self) -> Option<String> {
        use windows::Win32::System::Com::{
            CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED,
        };
        if !self.is_uia() || self.ptr == 0 {
            return None;
        }
        struct ComInit(bool);
        impl Drop for ComInit {
            fn drop(&mut self) {
                if self.0 {
                    unsafe { CoUninitialize() };
                }
            }
        }
        let init = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        if init.is_err() && init.0 != -2147417850 {
            return None;
        }
        let _com = ComInit(init.is_ok());
        let target = std::mem::ManuallyDrop::new(unsafe {
            IUIAutomationElement::from_raw(self.ptr as *mut _)
        });
        retained_value(&target)
    }
}

impl Clone for RetainedElement {
    fn clone(&self) -> Self {
        if self.ptr != 0 {
            unsafe {
                match self.kind {
                    SnapshotKind::Uia => {
                        let iface = IUIAutomationElement::from_raw(self.ptr as *mut _);
                        let dup = iface.clone();
                        std::mem::forget(iface);
                        std::mem::forget(dup);
                    }
                    SnapshotKind::Msaa => {
                        let iface = IAccessible::from_raw(self.ptr as *mut _);
                        let dup = iface.clone();
                        std::mem::forget(iface);
                        std::mem::forget(dup);
                    }
                }
            }
        }
        Self {
            ptr: self.ptr,
            kind: self.kind,
            center: self.center,
            rect: self.rect,
            msaa_role: self.msaa_role,
        }
    }
}

impl Drop for RetainedElement {
    fn drop(&mut self) {
        if self.ptr != 0 {
            unsafe {
                match self.kind {
                    SnapshotKind::Uia => drop(IUIAutomationElement::from_raw(self.ptr as *mut _)),
                    SnapshotKind::Msaa => drop(IAccessible::from_raw(self.ptr as *mut _)),
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotKind {
    Uia,
    Msaa,
}

pub struct CachedSnapshot {
    elements: Vec<RetainedElement>,
}

impl CachedSnapshot {
    pub fn from_nodes(nodes: &[UiaNode], kind: SnapshotKind) -> Self {
        Self {
            elements: nodes
                .iter()
                .filter(|node| node.element_index.is_some())
                .map(|node| RetainedElement {
                    ptr: node.element_ptr,
                    kind,
                    center: (node.center_x, node.center_y),
                    rect: node.rect,
                    msaa_role: node.msaa_role,
                })
                .collect(),
        }
    }
}

impl SnapshotPayload for CachedSnapshot {
    type Element = RetainedElement;

    fn len(&self) -> usize {
        self.elements.len()
    }

    fn retain(&self, index: usize) -> Option<Self::Element> {
        self.elements
            .get(index)
            .filter(|element| element.ptr != 0)
            .cloned()
    }
}

#[cfg(test)]
#[path = "cache_uaf_repro.rs"]
mod cache_uaf_repro;

#[cfg(test)]
pub(crate) mod keyboard_focus_owned_fixture_tests {
    use super::*;
    use std::cell::Cell;
    use std::sync::mpsc;
    use std::time::Duration;
    use windows::core::w;
    use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, WPARAM};
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
        COINIT_APARTMENTTHREADED,
    };
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::Accessibility::{CUIAutomation, IUIAutomation};
    use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, DispatchMessageW, GetForegroundWindow, GetWindowTextW,
        PeekMessageW, SendMessageW, SetForegroundWindow, TranslateMessage, MSG, PM_REMOVE,
        WINDOW_EX_STYLE, WS_CHILD, WS_OVERLAPPEDWINDOW, WS_TABSTOP, WS_VISIBLE,
    };

    enum FixtureCommand {
        Focus(bool),
        Observe(bool),
        Stop,
    }
    enum FixtureAcknowledgment {
        Focused,
        Observation {
            selection: (u16, u16),
            value: String,
        },
    }
    pub(crate) struct Fixture {
        pub(crate) top: u64,
        pub(crate) first: u64,
        pub(crate) second: u64,
        commands: mpsc::Sender<FixtureCommand>,
        acknowledged: mpsc::Receiver<FixtureAcknowledgment>,
        worker: Option<std::thread::JoinHandle<()>>,
    }
    impl Fixture {
        pub(crate) fn start() -> Self {
            let (ready_tx, ready_rx) = mpsc::channel();
            let (commands, receive) = mpsc::channel::<FixtureCommand>();
            let (ack_tx, acknowledged) = mpsc::channel();
            let worker = std::thread::spawn(move || unsafe {
                let module = GetModuleHandleW(None).unwrap();
                let instance = HINSTANCE(module.0);
                let top = CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    w!("STATIC"),
                    w!("CUA owned keyboard focus fixture"),
                    WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                    40,
                    40,
                    360,
                    180,
                    None,
                    None,
                    instance,
                    None,
                )
                .unwrap();
                let first = CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    w!("EDIT"),
                    w!("Fixture001"),
                    WS_CHILD | WS_VISIBLE | WS_TABSTOP,
                    15,
                    20,
                    280,
                    25,
                    top,
                    None,
                    instance,
                    None,
                )
                .unwrap();
                let second = CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    w!("EDIT"),
                    w!("SyntheticSibling"),
                    WS_CHILD | WS_VISIBLE | WS_TABSTOP,
                    15,
                    60,
                    280,
                    25,
                    top,
                    None,
                    instance,
                    None,
                )
                .unwrap();
                // Synthetic fixture setup puts the caret at the end, so Home
                // must produce an observed change rather than a trivial no-op.
                SendMessageW(first, 0x00b1, WPARAM(10), LPARAM(10)); // EM_SETSEL
                let _ = SetForegroundWindow(top);
                let _ = SetFocus(first);
                ready_tx
                    .send((
                        top.0 as usize as u64,
                        first.0 as usize as u64,
                        second.0 as usize as u64,
                    ))
                    .unwrap();
                loop {
                    match receive.try_recv() {
                        Ok(FixtureCommand::Focus(use_second)) => {
                            let _ = SetForegroundWindow(top);
                            let _ = SetFocus(if use_second { second } else { first });
                            ack_tx.send(FixtureAcknowledgment::Focused).unwrap();
                        }
                        Ok(FixtureCommand::Observe(use_second)) => {
                            // Only HWNDs created and owned on this thread are read.
                            // EM_GETSEL packed bounds are safe for this <64-char fixture;
                            // no pointer-bearing cross-thread diagnostic message is used.
                            let edit = if use_second { second } else { first };
                            let bounds = SendMessageW(edit, 0x00b0, WPARAM(0), LPARAM(0)).0 as u32;
                            let mut text = [0u16; 64];
                            let count = GetWindowTextW(edit, &mut text);
                            assert!(count >= 0 && (count as usize) < text.len());
                            ack_tx
                                .send(FixtureAcknowledgment::Observation {
                                    selection: ((bounds & 0xffff) as u16, (bounds >> 16) as u16),
                                    value: String::from_utf16_lossy(&text[..count as usize]),
                                })
                                .unwrap();
                        }
                        Ok(FixtureCommand::Stop) | Err(mpsc::TryRecvError::Disconnected) => break,
                        Err(mpsc::TryRecvError::Empty) => {}
                    }
                    let mut message = MSG::default();
                    while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                        let _ = TranslateMessage(&message);
                        DispatchMessageW(&message);
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                let _ = DestroyWindow(top);
            });
            let (top, first, second) = ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            Self {
                top,
                first,
                second,
                commands,
                acknowledged,
                worker: Some(worker),
            }
        }
        pub(crate) fn focus(&self, second: bool) {
            self.commands.send(FixtureCommand::Focus(second)).unwrap();
            assert!(matches!(
                self.acknowledged
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap(),
                FixtureAcknowledgment::Focused
            ));
            // Do not skip or silently pass a noninteractive CI environment.
            assert_eq!(
                unsafe { GetForegroundWindow() }.0 as usize as u64,
                self.top,
                "owned fixture requires an interactive Windows desktop"
            );
        }
    }
    impl Fixture {
        pub(crate) fn observe(&self, second: bool) -> ((u16, u16), String) {
            self.commands.send(FixtureCommand::Observe(second)).unwrap();
            match self
                .acknowledged
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
            {
                FixtureAcknowledgment::Observation { selection, value } => (selection, value),
                _ => panic!("unexpected owned-fixture acknowledgment"),
            }
        }
        pub(crate) fn wait_for_selection(&self, expected: (u16, u16)) {
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            loop {
                if self.observe(false).0 == expected {
                    return;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "owned Edit did not consume the requested selection key"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = self.commands.send(FixtureCommand::Stop);
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
    }
    pub(crate) struct TestCom(bool);
    impl TestCom {
        pub(crate) fn start() -> Self {
            let init = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
            assert!(init.is_ok() || init.0 == -2147417850);
            Self(init.is_ok())
        }
    }
    impl Drop for TestCom {
        fn drop(&mut self) {
            if self.0 {
                unsafe { CoUninitialize() };
            }
        }
    }
    pub(crate) fn retained_native(hwnd: u64) -> RetainedElement {
        // Caller keeps its apartment alive until every retained guard drops.
        let uia: IUIAutomation =
            unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) }.unwrap();
        let element = unsafe { uia.ElementFromHandle(HWND(hwnd as *mut _)) }.unwrap();
        let ptr = element.as_raw() as usize;
        std::mem::forget(element); // Transfer owned reference to cache guard.
        RetainedElement {
            ptr,
            kind: SnapshotKind::Uia,
            center: (0, 0),
            rect: None,
            msaa_role: None,
        }
    }
    #[test]
    #[ignore = "requires owned interactive Windows desktop; no actual SendInput"]
    fn owned_two_edit_exact_uia_and_gui_focus_positive_negative() {
        let fixture = Fixture::start();
        let _com = TestCom::start();
        let first = retained_native(fixture.first);
        let second = retained_native(fixture.second);
        let insert_calls = Cell::new(0);
        let pid = std::process::id();
        fixture.focus(false);
        let success = crate::input::keyboard::dispatch_retained_key_for_owned_fixture(
            fixture.top,
            pid,
            &first,
            &insert_calls,
        )
        .unwrap();
        assert_eq!(insert_calls.get(), 1);
        assert_eq!(success.uia_focused_element_equals_target, Some(true));
        assert_eq!(success.native_focus_stable, Some(true));
        // Native root/PID match but cached sibling has no exact UIA focus.
        let failure = crate::input::keyboard::dispatch_retained_key_for_owned_fixture(
            fixture.top,
            pid,
            &second,
            &insert_calls,
        )
        .unwrap_err();
        assert!(!failure.witness.input_attempted);
        assert_eq!(insert_calls.get(), 1);
        fixture.focus(true);
        crate::input::keyboard::dispatch_retained_key_for_owned_fixture(
            fixture.top,
            pid,
            &second,
            &insert_calls,
        )
        .unwrap();
        assert_eq!(insert_calls.get(), 2);
    }
}

#[cfg(test)]
mod input_scope_ownership_tests {
    use super::*;
    #[test]
    fn signed_negative_provider_pids_refuse_before_unsigned_conversion() {
        assert_eq!(checked_input_uia_pid(-1), None);
        assert_eq!(checked_input_uia_pid(i32::MIN), None);
    }
    #[test]
    fn zero_provider_pid_is_not_an_owner() {
        assert_eq!(checked_input_uia_pid(0), None);
    }
    #[test]
    fn signed_positive_provider_pid_bounds_are_preserved() {
        assert_eq!(checked_input_uia_pid(1), Some(1));
        assert_eq!(checked_input_uia_pid(i32::MAX), Some(i32::MAX as u32));
    }
    fn virtual_facts() -> InputOwnershipFacts {
        InputOwnershipFacts {
            direct_process: false,
            windowless: true,
            document_before_anchor: true,
            exact_boundary_reached: true,
            non_root_native_child: true,
            anchor_element_from_handle_equal: true,
            anchor_provider_matches_native_owner: true,
            provider_pid_nonzero: true,
        }
    }
    #[test]
    fn direct_native_process_retains_its_existing_proof_path() {
        let mut f = virtual_facts();
        f.direct_process = true;
        f.windowless = false;
        f.document_before_anchor = false;
        f.anchor_element_from_handle_equal = false;
        assert_eq!(
            classify_input_ownership(f),
            Some(InputUiaOwnership::DirectProcess)
        );
    }
    #[test]
    fn differing_leaf_pid_admitted_only_with_complete_virtual_ownership() {
        assert_eq!(
            classify_input_ownership(virtual_facts()),
            Some(InputUiaOwnership::VirtualDocument)
        );
    }
    #[test]
    fn same_pid_document_still_tracks_the_narrow_virtual_path() {
        let mut f = virtual_facts();
        f.direct_process = true;
        assert_eq!(
            classify_input_ownership(f),
            Some(InputUiaOwnership::VirtualDocument)
        );
    }
    #[test]
    fn wrong_exact_root_refuses_even_direct_matching_pid() {
        let mut f = virtual_facts();
        f.exact_boundary_reached = false;
        f.direct_process = true;
        assert_eq!(classify_input_ownership(f), None);
    }
    #[test]
    fn broad_frame_anchor_is_not_a_virtual_document_proof() {
        let mut f = virtual_facts();
        f.non_root_native_child = false;
        assert_eq!(classify_input_ownership(f), None);
    }
    #[test]
    fn native_control_cannot_borrow_virtual_document_relaxation() {
        let mut f = virtual_facts();
        f.windowless = false;
        assert_eq!(classify_input_ownership(f), None);
    }
    #[test]
    fn document_only_above_a_native_control_cannot_authorize_virtual_input() {
        let mut f = virtual_facts();
        f.document_before_anchor = false;
        assert_eq!(classify_input_ownership(f), None);
    }
    #[test]
    fn provider_claimed_native_handle_without_fresh_identity_refuses() {
        let mut f = virtual_facts();
        f.anchor_element_from_handle_equal = false;
        assert_eq!(classify_input_ownership(f), None);
    }
    #[test]
    fn native_host_provider_owner_mismatch_refuses() {
        let mut f = virtual_facts();
        f.anchor_provider_matches_native_owner = false;
        assert_eq!(classify_input_ownership(f), None);
    }
    #[test]
    fn zero_provider_identity_cannot_be_relabelled_owned() {
        let mut f = virtual_facts();
        f.provider_pid_nonzero = false;
        f.direct_process = true;
        assert_eq!(classify_input_ownership(f), None);
    }
}
