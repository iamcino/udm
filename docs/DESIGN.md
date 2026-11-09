# UDM interface and product design

UDM should make a long download easy to start, easy to understand and safe
to leave running. The interface gives the files the space; transport and
recovery details appear when they help explain what is happening.

This guide describes the implemented desktop design at `241f550`.
[Architecture](ARCHITECTURE.md) explains its Rust/Slint boundaries;
[Operations](OPERATIONS.md) is the user guide.

## Product principles

- **A utility earns its space.** No advertisements, promotional panels,
  account requirement or engagement prompts. The main content is the task list.
- **Privacy is the default.** No telemetry switch is needed because there
  is no telemetry implementation. Diagnostics remain local.
- **Stay within the user's account.** No elevation or service is required.
  Startup and browser registration are disclosed per-user integrations.
- **Show evidence, not promises.** A protocol badge names the transport in
  use; a segment count is not presented as a physical connection count.
  Unknown information should not look like a measured zero.
- **Make background work predictable.** Closing, minimizing, starting hidden
  and starting with Windows are separate choices. A recovery record must
  remain visible without pretending it is a runnable task.

## Visual language

The interface uses Slint with the vendored cino-ui components and theme.
Its quiet surfaces, source-list navigation and grouped settings draw from
the Apple HIG style shared by the project's companion apps, adapted to
Windows 11. It is not a claim that the app uses native Windows controls
for every element.

`crates/gui/ui/theme.slint` adapts UDM token names to
`vendor/cino-ui/ui/theme.slint`. The shared theme owns surfaces, text,
spacing-related shapes and the **Be Vietnam Pro** font. UDM owns table
geometry and supplies five accent choices with light-mode contrast
adjustments: blue, green, orange, pink and purple.

| Element | Design role |
|---|---|
| Window, sidebar and grouped surfaces | Separate navigation, content and controls through surface tone. |
| Accent | Primary actions, selection and active progress. |
| Semantic colours | Success, warning and failure, reinforced by text labels. |
| Thin linear progress | Communicate transfer progress without a large gauge. |
| Typography | Clear task names, quieter metadata, compact labels and units. |
| Rounded groups and controls | Follow shared theme radii rather than view-specific shapes. |

Dark, Light and Auto are implemented. Auto reads Windows' app colour
preference and resolves the same light/dark tokens; it is refreshed while
the application runs. The native tray menu follows the operating system's
menu appearance rather than an independently forced Slint theme.

## Main window

The sidebar contains library filters, the Add Download form, Settings,
About and an entry for the active-downloads popup. Its storage display
describes the download volume. The main toolbar provides quick-add,
filtering and **Clear finished**. The task table carries file identity,
size, state, progress, speed and added time.

Rows combine a filename-extension badge, a readable name and compact
transport/segment/checksum metadata. Actions are state-dependent. The
table supports row selection, bulk operations, a right-click menu and
resizable columns; double-clicking a completed row opens its file. A task
detail view exposes information that would make the table too dense.

The layout must remain usable at the supported minimum window size, at
normal and maximized sizes, and with bulk selection active. Clear finished
and row controls must remain visible and clickable. Long names, translated
labels and narrow columns need truncation or wrapping that preserves actions.

## Common tasks

| To… | Use… |
|---|---|
| Start a direct file download | Paste its URL in the toolbar and press Enter. |
| Add several links or supply a checksum | Add Download; one URL per line. |
| Choose a destination | The Add form's folder control. |
| Find or narrow the list | Text filter and All/Active/Completed/Failed library views. |
| Pause, resume, cancel, retry or remove | Eligible row, context-menu or bulk actions. |
| Inspect one task | Its detail view. |
| Open a finished file or its folder | The task's available file/folder actions. |
| Change appearance or background behaviour | Settings. |
| Glance at ongoing work | Active-downloads popup from the tray menu or sidebar. |

State labels accompany colour: active work uses the accent, paused/recovery
uses warning, completed uses success and failed uses danger. Unknown-size
progress remains indeterminate. Errors are visible through row/detail
information and transient feedback; they do not disappear into a console
that the desktop user cannot see.

## Tray, popup and lifecycle

The tray tooltip and menu summaries show live download totals and speed.
Pause all, resume all, show/hide, active downloads and Exit provide a small
set of explicit actions. A single tray click is neutral; double-click shows
the main window. The popup opens only through an explicit tray-menu or
sidebar action, not automatically on a download event.

The borderless popup is positioned within the work area and includes totals,
download rows, pause/resume controls and a labelled **Open UDM** action.
Paused tasks are included so they can be resumed there. Focus loss closes
the popup; it is a glanceable control surface rather than a second main window.

| Setting | Default | Meaning |
|---|---|---|
| Close to tray | On | Closing hides the window and keeps the app running. |
| Minimize to tray | Off | Minimizing can hide the window to the tray. |
| Start minimized | Off | Launch with the main window hidden. |
| Start with Windows | Off | Register a per-user HKCU Run entry. |

Exit in the tray menu always quits. Startup settings explain the registry
write; the UI reads actual registry presence when showing the toggle and
warns about an executable under Temp or Downloads. Startup synchronization
uses the current executable path, so a moved portable copy can update its
entry on a later launch.

## Categories and browser recovery

Category routing is a destination policy, not a separate category library
in the sidebar. It is off by default. When enabled, eligible new tasks use
filename suffixes to select a subfolder. The preview and engine share the
classification code; already-locked tasks are not moved when settings change.

Browser handoff has its own ownership boundary: prepare must be durable
before browser cancellation, and commit follows confirmed cancellation.
An unresolved committed task association appears as **Needs recovery** in
the All list. These rows are read-only and filterable, with no engine
selection or action capabilities. The warning label describes uncertainty;
it must not imply that clicking Retry will repair it.

The extension also presents recovery outcomes, including an expired commit.
That extension status is broader than the GUI's pending-association rows.
The UI must not promise that every interrupted handoff produces a GUI row
or that the browser still has the file after cancellation.

## Language and verification

English and Vietnamese are implemented through the GUI's Rust strings and
Slint translation boundary. Protocol diagnostics can remain verbatim; task
IDs, category IDs and physical folder names do not change with language.
Popup text is supplied independently from Rust rather than assuming the main
window's translation callback exists on another window instance.

For a UI change, compile both Slint paths and inspect screenshots with
completed, active, paused, failed and recovery data. Check both themes,
long English/Vietnamese labels, resizing, selection and unobscured controls.
Compilation alone cannot establish that the layout is readable.

## Scope beyond this interface

No global speed limit, timed queue scheduling, torrent support or media
resolver is implemented. The authenticated updater, N2 dual-stack (IPv4/IPv6
selection) and N3 multi-family transfers (splitting one download across
address families) are design-only, not built. N2/N3 implementation remains
measurement-gated. None should appear as a working setting or a shipped benefit.
