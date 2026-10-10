# Foreground target ownership: Issue344 scoped candidate

This source-owned derivative is not an official release. Its exact base is
`891efae2cc01a38260a303b0329785e5c4d7b03c`; Windows compilation, real provider
acceptance and controlled Excel acceptance are separate gates. This document
does not assert any of them passed.

## Native ownership policy

The admitted foreground root must keep its exact HWND, owner PID, thread and
native ancestry. An owner window, a sibling window, a matching process name, an
application class, a process family or a shared desktop is insufficient.

For a retained UIA key, `HasKeyboardFocus` and `CompareElements` against actual
UIA focus remain hard requirements. The provider leaf/focused PIDs must be
strictly positive as signed UIA properties before any unsigned conversion,
equal and stable. Negative i32 bit patterns are never native owner IDs. Complete bounded RawView ancestry must reach the
fresh exact admitted root. Ordinary native controls keep direct PID ownership
and exact native focus correlation.

The private `VirtualDocument` proof path additionally requires all of:

- The addressed UIA target is windowless.
- Its raw chain contains an actual UIA Document before its first native anchor.
- That anchor is a real non-root physical child of the admitted root, with
  exact `IsChild` and `GA_ROOT` membership.
- The raw anchor object equals fresh `ElementFromHandle(anchor)`.
- The anchor object's UIA PID equals the actual native anchor owner PID.
- Document identity, anchor identity, leaf process and native anchor facts
  survive refresh, including any optional private ValuePattern equality read.
- Final native foreground/thread/focus identities remain stable, and actual
  native focus lies within that exact anchor.

No provider IDs, HWNDs, PIDs, names, text values, hashes of values or raw provider
errors are added to the diagnostic payload. COM interface release and apartment
teardown finish before the final native-only checks and the existing insertion.
External OS/provider races are not made atomic by these observations.

## Wheel route distinction

A native list, tree, grid or Excel SDM-style control retains the precise
focus-inside-hovered-scroll-scope check. The driver never clicks, changes focus,
selects a row or retries to satisfy it.

Only a fully proved windowless Document fragment may use hover semantics with
keyboard focus elsewhere in that same exact document and native child anchor.
Unrelated documents, sibling native anchors and foreign roots refuse before
wheel insertion. Anchor ownership and hover/focus physical containment are
checked against the latest native sample, not only the initial sample. An
intermediate ancestor reparent is a refusal even when leaf HWNDs/root are
unchanged. A missing Document/anchor proof never becomes a broad frame permit.

## SDK and adapter contract clarification

`CUA_KEYBOARD_FOCUS_WITNESS_V1` keeps exactly the existing 18 required fields,
the same closed enums and the same fixed maximum payload size. This candidate
explicitly extends the meaning of `uiaProcessMatchesTarget`: true means **proved
process ownership within the admitted target boundary**, through direct numeric
PID equality **or** the complete `VirtualDocument` relation above. It does not
assert that a virtual leaf PID numerically equals the frame PID. The proof-path
enum is private; a decoder cannot infer numeric equality from the boolean.

The OpenClaw standard retained-element foreground key adapter still requires
that boolean to be true, along with every existing focus/identity predicate. A
false/unproved ownership value on a claimed success remains invalid. No parser,
schema, API parameter, fallback, actuator or retry policy is loosened.

The SDK public result remains `Unverifiable` / `GlobalInput` / `Foreground`.
`delivery.deliveredCount` and `insertedEventCount` are the same actual requested
key insertion count. They are not activation/focus click counts, semantic
recipient proof, field consumption, full selection, replacement or Excel effect
proof. The numeric reader preserves native provider HRESULT failures and otherwise
uses the existing process-ownership refusal for an invalid signed PID; neither
raw numbers nor provider text enter the witness. The marker still contains no native identity or proof-path enum. Private
fixed native refusal subreasons precede the marker, without changing the wire.

## Required acceptance (not a PASS report)

1. Pin Rust1.97.1 and Windows MSVC, compile production and test targets.
2. Run the original native unit/SDK gates, the 27 new source-bound policy,
   dispatch, anchor-drift and privacy cases, and the two existing owned two-Edit
   GUI fixtures. Portable extraction is only helper proof, not Win32/UIA proof.
3. Run all 86 unchanged canonical cells with their original application effect
   oracles, 0 typed skips and complete video/trajectory evidence. The actual
   Electron/Tauri positives supply real virtual provider acceptance only if the
   corrected gate and the original effect oracles both pass. Do not relabel
   their expected deliveries as permitted refusals.
4. Preserve native control/SDM sibling-focus, wrong-root and ownership refusals.
5. Rebuild and rebake a new complete derivative package from accepted source;
   validate its SDK import and canonical package layout. Old DLL/package hashes
   do not represent this successor.
6. After separately controlled activation, perform synthetic standard-tool
   Excel selection/replacement/scroll acceptance and readback. This source
   compatibility change is not an asserted remedy for the original Excel issue.
