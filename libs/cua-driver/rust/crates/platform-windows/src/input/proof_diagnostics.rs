//! Private, default-off refusal diagnostics for synthetic owning-fixture CI.
//! Raw identities are transient inputs, never Debug/Serialize or output fields.
//! No caller invokes this observer on an accepted path or authorizes input with it.

use std::fmt::Write as _;

const MARKER: &str = "CUA_NATIVE_INPUT_PROOF_DIAGNOSTIC_V1 ";
const MAX_NODES: usize = 64;
const MAX_DEPTH: usize = 64;
const MAX_BYTES: usize = 16 * 1024;

#[derive(Clone, Copy)]
pub(super) struct Scope {
    pub own: u64,
    pub anchor: u64,
    pub parent: u64,
    pub root: u64,
    pub pid: u32,
    pub thread: u32,
    pub provider_pid: u32,
    pub virtual_document: bool,
}

#[cfg(all(feature = "native-input-proof-diagnostics", target_os = "windows"))]
impl From<crate::uia::cache::InputUiaScopeWitness> for Scope {
    fn from(s: crate::uia::cache::InputUiaScopeWitness) -> Self {
        Self {
            own: s.own_native_hwnd,
            anchor: s.nearest_native_hwnd,
            parent: s.anchor_parent,
            root: s.anchor_root,
            pid: s.anchor_pid,
            thread: s.anchor_thread,
            provider_pid: s.provider_pid,
            virtual_document: s.ownership == crate::uia::cache::InputUiaOwnership::VirtualDocument,
        }
    }
}

#[derive(Clone, Copy)]
pub(super) struct Snapshot {
    pub target: u64,
    pub root: u64,
    pub target_pid: u32,
    pub target_thread: u32,
    pub root_thread: u32,
    pub foreground: u64,
    pub active: Option<u64>,
    pub focus: u64,
    pub hover: Option<u64>,
    pub focus_pid: Option<u32>,
    pub focus_thread: Option<u32>,
    pub point: Option<(i32, i32)>,
    pub scope: Scope,
}

#[derive(Clone, Copy)]
pub(super) enum Predicates {
    Key {
        anchor_identity: bool,
        focus_within_anchor: bool,
    },
    Wheel {
        anchor_identity: bool,
        hover_within_anchor: bool,
        focus_within_anchor: bool,
    },
}
impl Predicates {
    fn operation(self) -> &'static str {
        match self {
            Self::Key { .. } => "retained_key",
            Self::Wheel { .. } => "virtual_wheel",
        }
    }
    fn json(self) -> String {
        match self {
            Self::Key { anchor_identity, focus_within_anchor } => format!(
                "{{\"anchorIdentityMatches\":{anchor_identity},\"focusWithinAnchor\":{focus_within_anchor}}}"),
            Self::Wheel { anchor_identity, hover_within_anchor, focus_within_anchor } => format!(
                "{{\"anchorIdentityMatches\":{anchor_identity},\"hoverWithinAnchor\":{hover_within_anchor},\"focusWithinAnchor\":{focus_within_anchor}}}"),
        }
    }
}

// A separate observation AFTER rejection; it is not the original guard sample.
#[derive(Clone, Copy, Default)]
struct PostSample {
    available: bool,
    foreground: u64,
    active: u64,
    focus: u64,
    hover: Option<u64>,
    target_pid: u32,
    target_thread: u32,
    target_root: u64,
    cursor_matches_gate: Option<bool>,
}
#[derive(Clone, Copy, Default)]
struct RawNode {
    parent: u64,
    root: u64,
    pid: u32,
    thread: u32,
    readable: bool,
}
#[derive(Default)]
struct Classes<T>(Vec<T>);
impl<T: Copy + PartialEq> Classes<T> {
    fn class(&mut self, value: T) -> usize {
        if let Some(i) = self.0.iter().position(|v| *v == value) {
            i
        } else {
            let i = self.0.len();
            self.0.push(value);
            i
        }
    }
}
fn index_json(value: Option<usize>) -> String {
    value.map_or_else(|| "null".to_owned(), |i| i.to_string())
}
fn intern(handles: &mut Vec<u64>, value: u64, node_bound: &mut bool) -> Option<usize> {
    if value == 0 {
        return None;
    }
    if let Some(i) = handles.iter().position(|h| *h == value) {
        return Some(i);
    }
    if handles.len() == MAX_NODES {
        *node_bound = true;
        return None;
    }
    let i = handles.len();
    handles.push(value);
    Some(i)
}

/// The callback only returns bounded native primitives. Production calls it
/// after refusal. Tests use generated graphs, never a fake Windows acceptance.
fn encode(
    gate: Snapshot,
    predicates: Predicates,
    post: PostSample,
    mut observe: impl FnMut(u64) -> RawNode,
) -> String {
    let mut handles = Vec::with_capacity(MAX_NODES);
    let mut node_bound = false;
    let gate_values = [
        gate.target,
        gate.root,
        gate.foreground,
        gate.active.unwrap_or(0),
        gate.focus,
        gate.hover.unwrap_or(0),
        gate.scope.own,
        gate.scope.anchor,
        gate.scope.parent,
    ];
    let gate_aliases: Vec<_> = gate_values
        .into_iter()
        .map(|h| intern(&mut handles, h, &mut node_bound))
        .collect();
    let post_values = [
        post.foreground,
        post.active,
        post.focus,
        post.hover.unwrap_or(0),
    ];
    let post_aliases: Vec<_> = post_values
        .into_iter()
        .map(|h| intern(&mut handles, h, &mut node_bound))
        .collect();
    let mut processes = Classes::<u32>::default();
    let mut threads = Classes::<u32>::default();
    let provider_class = processes.class(gate.scope.provider_pid);
    let anchor_pid_class = processes.class(gate.scope.pid);
    let anchor_thread_class = threads.class(gate.scope.thread);
    let mut nodes = Vec::with_capacity(MAX_NODES);
    let mut parents = Vec::with_capacity(MAX_NODES);
    let mut unavailable = !post.available;
    let mut anchor_matches = false;
    let mut focus_matches = true;
    let mut slot = 0;
    while slot < handles.len() && slot < MAX_NODES {
        let h = handles[slot];
        let raw = observe(h);
        let within_root = raw.root == gate.root;
        unavailable |= !raw.readable || !within_root;
        if h == gate.scope.anchor {
            anchor_matches = raw.readable
                && raw.parent == gate.scope.parent
                && raw.root == gate.scope.root
                && raw.pid == gate.scope.pid
                && raw.thread == gate.scope.thread;
        }
        if h == gate.focus {
            focus_matches = raw.readable
                && within_root
                && gate.focus_pid.is_none_or(|pid| raw.pid == pid)
                && gate.focus_thread.is_none_or(|thread| raw.thread == thread);
        }
        // Do not traverse an owner/ancestor outside the admitted root. A drifted
        // focus/anchor is classified incomplete, never used to observe a foreign tree.
        let parent = if within_root && h != gate.root {
            intern(&mut handles, raw.parent, &mut node_bound)
        } else {
            None
        };
        let root = if within_root {
            intern(&mut handles, raw.root, &mut node_bound)
        } else {
            None
        };
        parents.push(parent);
        nodes.push(format!(
            "{{\"alias\":{slot},\"parent\":{},\"root\":{},\"processClass\":{},\"threadClass\":{},\"readable\":{},\"withinAdmittedRoot\":{}}}",
            index_json(parent), index_json(root), processes.class(raw.pid), threads.class(raw.thread), raw.readable, within_root));
        slot += 1;
    }
    let mut cycle = false;
    let mut depth_bound = false;
    for start in 0..parents.len() {
        let mut visited = [false; MAX_NODES];
        let mut current = Some(start);
        for _ in 0..MAX_DEPTH {
            let Some(i) = current else {
                break;
            };
            if visited[i] {
                cycle = true;
                current = None;
                break;
            }
            visited[i] = true;
            current = parents.get(i).copied().flatten();
        }
        if current.is_some() {
            depth_bound = true;
        }
    }
    let matches_gate = post.available
        && post.foreground == gate.foreground
        && post.focus == gate.focus
        && gate.active.is_none_or(|active| post.active == active)
        && post.hover == gate.hover
        && post.target_pid == gate.target_pid
        && post.target_thread == gate.target_thread
        && post.target_root == gate.root
        && (gate.point.is_none() || post.cursor_matches_gate == Some(true))
        && anchor_matches
        && focus_matches;
    let complete = !unavailable && !node_bound && !depth_bound && !cycle;
    let mut out = String::with_capacity(8192);
    let _ = write!(out,
        "{{\"schema\":\"cua-native-input-proof-diagnostic/v1\",\"operation\":\"{}\",\"phase\":\"after_refusal\",\"inputAttempted\":false,\"gate\":{{\"predicates\":{},\"aliases\":{{\"target\":{},\"root\":{},\"foreground\":{},\"active\":{},\"focus\":{},\"hover\":{},\"ownNative\":{},\"providerAnchor\":{},\"anchorParent\":{}}},\"scope\":{{\"virtualDocument\":{},\"windowless\":{},\"nonRootNativeAnchor\":{},\"providerPidPositive\":{},\"providerProcessClass\":{},\"anchorProcessClass\":{},\"anchorThreadClass\":{}}}}},\"additionalObservation\":{{\"matchesGateSnapshot\":{},\"traversalBoundary\":\"admittedRoot\",\"cursorMatchesGate\":{},\"status\":\"{}\",\"unavailable\":{},\"nodeBound\":{},\"depthBound\":{},\"cycle\":{},\"aliases\":{{\"foreground\":{},\"active\":{},\"focus\":{},\"hover\":{}}},\"nodes\":[{}]}}}}",
        predicates.operation(), predicates.json(),
        index_json(gate_aliases[0]), index_json(gate_aliases[1]), index_json(gate_aliases[2]),
        index_json(gate_aliases[3]), index_json(gate_aliases[4]), index_json(gate_aliases[5]),
        index_json(gate_aliases[6]), index_json(gate_aliases[7]), index_json(gate_aliases[8]),
        gate.scope.virtual_document, gate.scope.own == 0, gate.scope.anchor != gate.scope.root,
        gate.scope.provider_pid > 0, provider_class, anchor_pid_class, anchor_thread_class,
        matches_gate, post.cursor_matches_gate.map_or_else(|| "null".to_owned(), |v| v.to_string()), if complete { "complete" } else { "incomplete" }, unavailable, node_bound, depth_bound, cycle,
        index_json(post_aliases[0]), index_json(post_aliases[1]), index_json(post_aliases[2]), index_json(post_aliases[3]), nodes.join(","));
    // The schema contains only fixed names/enums, booleans and anonymous indices.
    // Keep a truthful bounded refusal even if a future schema grows unexpectedly.
    if out.len() + MARKER.len() + 1 > MAX_BYTES {
        return format!("{{\"schema\":\"cua-native-input-proof-diagnostic/v1\",\"operation\":\"{}\",\"phase\":\"after_refusal\",\"inputAttempted\":false,\"gate\":{{\"predicates\":{}}},\"additionalObservation\":{{\"status\":\"incomplete\",\"byteBound\":true}}}}", predicates.operation(), predicates.json());
    }
    out
}

#[cfg(all(feature = "native-input-proof-diagnostics", target_os = "windows"))]
pub(super) fn record_refusal(gate: Snapshot, predicates: Predicates) {
    use std::io::Write as _;
    use windows::Win32::Foundation::{HWND, POINT};
    use windows::Win32::UI::WindowsAndMessaging::{
        GetAncestor, GetCursorPos, GetForegroundWindow, GetGUIThreadInfo, GetParent,
        GetWindowThreadProcessId, IsWindow, WindowFromPoint, GA_ROOT, GUITHREADINFO,
    };
    let target = HWND(gate.target as *mut _);
    let mut target_pid = 0;
    let target_thread = unsafe { GetWindowThreadProcessId(target, Some(&mut target_pid)) };
    let mut info = GUITHREADINFO {
        cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
        ..Default::default()
    };
    let available = unsafe { GetGUIThreadInfo(gate.root_thread, &mut info) }.is_ok();
    let post = PostSample {
        available,
        foreground: unsafe { GetForegroundWindow() }.0 as usize as u64,
        active: info.hwndActive.0 as usize as u64,
        focus: info.hwndFocus.0 as usize as u64,
        hover: gate
            .point
            .map(|(x, y)| unsafe { WindowFromPoint(POINT { x, y }) }.0 as usize as u64),
        target_pid,
        target_thread,
        target_root: unsafe { GetAncestor(target, GA_ROOT) }.0 as usize as u64,
        cursor_matches_gate: gate.point.and_then(|(x, y)| {
            let mut cursor = POINT::default();
            unsafe { GetCursorPos(&mut cursor) }
                .ok()
                .map(|_| cursor.x == x && cursor.y == y)
        }),
    };
    let record = encode(gate, predicates, post, |h| {
        let window = HWND(h as *mut _);
        let root = unsafe { GetAncestor(window, GA_ROOT) }.0 as usize as u64;
        if root != gate.root {
            return RawNode {
                root,
                ..Default::default()
            };
        }
        let mut pid = 0;
        let thread = unsafe { GetWindowThreadProcessId(window, Some(&mut pid)) };
        let (parent, parent_readable) = match unsafe { GetParent(window) } {
            Ok(parent) => (parent.0 as usize as u64, true),
            Err(_) if root == h => (0, true),
            Err(_) => (0, false),
        };
        RawNode {
            parent,
            root,
            pid,
            thread,
            readable: unsafe { IsWindow(window) }.as_bool()
                && pid != 0
                && thread != 0
                && parent_readable,
        }
    });
    // One bounded write, after refusal. A closed stderr cannot alter the error.
    let _ = std::io::stderr()
        .lock()
        .write_all(format!("{MARKER}{record}\n").as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sample() -> Snapshot {
        Snapshot {
            target: 90_000_001,
            root: 90_000_001,
            target_pid: 90_000_101,
            target_thread: 90_000_201,
            root_thread: 90_000_201,
            foreground: 90_000_001,
            active: Some(90_000_001),
            focus: 90_000_003,
            hover: Some(90_000_004),
            focus_pid: Some(90_000_102),
            focus_thread: Some(90_000_202),
            point: None,
            scope: Scope {
                own: 0,
                anchor: 90_000_002,
                parent: 90_000_001,
                root: 90_000_001,
                pid: 90_000_102,
                thread: 90_000_202,
                provider_pid: 90_000_103,
                virtual_document: true,
            },
        }
    }
    fn post(g: Snapshot) -> PostSample {
        PostSample {
            available: true,
            foreground: g.foreground,
            active: g.active.unwrap(),
            focus: g.focus,
            hover: g.hover,
            target_pid: g.target_pid,
            target_thread: g.target_thread,
            target_root: g.root,
            cursor_matches_gate: None,
        }
    }
    fn node(h: u64) -> RawNode {
        RawNode {
            parent: if h == 90_000_001 {
                0
            } else if h == 90_000_002 {
                90_000_001
            } else {
                90_000_002
            },
            root: 90_000_001,
            pid: if h == 90_000_001 {
                90_000_101
            } else {
                90_000_102
            },
            thread: if h == 90_000_001 {
                90_000_201
            } else {
                90_000_202
            },
            readable: true,
        }
    }
    fn wheel() -> Predicates {
        Predicates::Wheel {
            anchor_identity: true,
            hover_within_anchor: true,
            focus_within_anchor: false,
        }
    }
    #[test]
    fn identities_are_anonymous_but_equality_classes_and_predicates_survive() {
        let g = sample();
        let record = encode(g, wheel(), post(g), node);
        for raw in [
            90_000_001, 90_000_002, 90_000_003, 90_000_004, 90_000_101, 90_000_102, 90_000_103,
            90_000_201, 90_000_202,
        ] {
            assert!(!record.contains(&raw.to_string()));
        }
        for name in [
            "runtimeId",
            "className",
            "value",
            "title",
            "url",
            "environment",
        ] {
            assert!(!record.contains(name));
        }
        assert!(record.contains("\"focusWithinAnchor\":false"));
        assert!(record.contains("\"providerProcessClass\":0,\"anchorProcessClass\":1"));
        assert!(record.contains("\"matchesGateSnapshot\":true"));
        assert!(record.contains("\"status\":\"complete\""));
        println!("CUA_NATIVE_INPUT_ENCODER_TEST_FIXTURE_V1 {record}");
    }
    #[test]
    fn each_original_failed_wheel_predicate_is_distinct_and_preserved() {
        let g = sample();
        for (p, needle) in [
            (
                Predicates::Wheel {
                    anchor_identity: false,
                    hover_within_anchor: true,
                    focus_within_anchor: true,
                },
                "\"anchorIdentityMatches\":false",
            ),
            (
                Predicates::Wheel {
                    anchor_identity: true,
                    hover_within_anchor: false,
                    focus_within_anchor: true,
                },
                "\"hoverWithinAnchor\":false",
            ),
            (wheel(), "\"focusWithinAnchor\":false"),
        ] {
            assert!(encode(g, p, post(g), node).contains(needle));
        }
    }
    #[test]
    fn refusal_graph_never_becomes_an_input_attempt_or_ownership_permit() {
        let g = sample();
        let p = Predicates::Key {
            anchor_identity: true,
            focus_within_anchor: false,
        };
        let record = encode(g, p, post(g), node);
        assert!(record.contains("\"inputAttempted\":false"));
        assert!(record.contains("\"focusWithinAnchor\":false"));
        assert!(!record.contains("delivered"));
        println!("CUA_NATIVE_INPUT_ENCODER_KEY_TEST_FIXTURE_V1 {record}");
    }
    #[test]
    fn post_refusal_focus_or_anchor_parent_drift_is_not_guard_evidence() {
        let g = sample();
        let mut changed = post(g);
        changed.focus += 1;
        assert!(encode(g, wheel(), changed, node).contains("\"matchesGateSnapshot\":false"));
        assert!(encode(g, wheel(), post(g), |h| {
            let mut n = node(h);
            if h == g.scope.anchor {
                n.parent += 7;
            }
            n
        })
        .contains("\"matchesGateSnapshot\":false"));
    }
    #[test]
    fn cyclic_native_parent_graph_is_incomplete_and_observed_once_per_node() {
        let g = sample();
        let mut calls = 0;
        let record = encode(g, wheel(), post(g), |h| {
            calls += 1;
            let mut n = node(h);
            n.parent = h;
            n
        });
        assert_eq!(calls, 4);
        assert!(record.contains("\"cycle\":true"));
        assert!(record.contains("\"status\":\"incomplete\""));
    }
    #[test]
    fn hostile_unbounded_parent_chain_hits_node_and_byte_bounds() {
        let g = sample();
        let mut calls = 0;
        let record = encode(g, wheel(), post(g), |h| {
            calls += 1;
            RawNode {
                parent: h + 1,
                root: g.root,
                pid: 99_000_000 + calls,
                thread: 98_000_000 + calls,
                readable: true,
            }
        });
        assert_eq!(calls, MAX_NODES as u32);
        assert!(record.contains("\"nodeBound\":true"));
        assert!(record.len() + MARKER.len() + 1 <= MAX_BYTES);
        assert!(record.contains("\"status\":\"incomplete\""));
    }
    #[test]
    fn unavailable_native_observation_is_explicit_not_synthetic_success() {
        let g = sample();
        let record = encode(g, wheel(), PostSample::default(), |_| RawNode::default());
        assert!(record.contains("\"unavailable\":true"));
        assert!(record.contains("\"matchesGateSnapshot\":false"));
        assert!(record.contains("\"status\":\"incomplete\""));
    }
    #[test]
    fn foreign_native_root_is_incomplete_without_traversing_its_parent() {
        let g = sample();
        let mut seen = Vec::new();
        let record = encode(g, wheel(), post(g), |h| {
            seen.push(h);
            let mut n = node(h);
            if h == g.focus {
                n.root = 91_000_999;
                n.parent = 91_000_998;
            }
            n
        });
        assert!(!seen.contains(&91_000_999));
        assert!(!seen.contains(&91_000_998));
        assert!(record.contains("\"withinAdmittedRoot\":false"));
        assert!(record.contains("\"matchesGateSnapshot\":false"));
        assert!(record.contains("\"status\":\"incomplete\""));
    }
}
