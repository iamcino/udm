# udm — Design Document

This document describes the product design and the visual/interaction language
of the udm desktop UI. It is the source of truth for *why the app looks the
way it does*; `ARCHITECTURE.md` covers how it is built.

## 1. Product principles

1. **A download manager is a utility, not a billboard.** No ads, no "upgrade"
   surfaces, no gamification. The only content is the user's files.
2. **Zero telemetry is a design feature.** The UI states it plainly
   (`private` badge, status bar, About view) because it is verifiable from the
   build (see `PRIVACY.md`, `deny.toml`, `tests/no_telemetry.rs`).
3. **No administrator rights, ever.** Everything the app touches lives under
   `%LOCALAPPDATA%\udm` (or beside the exe in portable mode). The design
   must never imply system-level access (no "install for all users" affordances).
4. **HTTP/3 first, invisibly.** QUIC is a transport detail: the UI surfaces it
   as a small badge per row (`HTTP/3 (QUIC)`) and one global switch — never as
   a workflow the user must manage.
5. **Calm progress.** Downloads are long-running background work. Motion and
   colour are reserved for state changes; idle screens are quiet.

## 2. Design language (Apple HIG, adapted for Windows 11)

The UI follows the Apple Human Interface Guidelines dark-mode surface model
(macOS Sonoma/Sequoia era), rendered with Segoe UI Variable on Windows:

* **Depth through lightness, not borders.** Three surface levels do all the
  work: sidebar `#18181B` (recessed) → canvas `#1E1E22` → elevated card
  `#27272C`. Hairlines (`#FFFFFF14`, 1 px) appear only between large regions
  (toolbar/content, content/status bar) — never around every control.
* **One accent.** System blue `#0A84FF` marks exactly three things: the
  primary action, the focused field, and in-flight progress. Semantic colours
  are fixed: success `#30D158`, warning `#FF9F0A`, danger `#FF453A`.
* **Three text tiers.** `#F4F4F6` primary / `#8E8E93` secondary / `#636366`
  tertiary. Nothing else.
* **Compact radii.** 10 px cards/popovers, 7–8 px buttons/inputs/rows, 2 px
  progress bars.
* **Flat everything.** No gradients, no drop shadows inside the window, no
  skeuomorphism, no gauge/dial widgets.

Design tokens live in `crates/gui/ui/theme.slint` (single global `Theme`); no
view is allowed to hard-code a colour.

### 2.1 Things this design deliberately removed

Earlier concept mockups (Dribbble-style "IDM" looks) contained: a double
header (window title bar + in-app title bar), a semi-circular disk gauge,
promotional cards, gradient buttons, and per-row divider lines. All of them
are banned by this document: the window chrome already carries the title, a
linear 4 px bar communicates free space better than a dial, and dividers add
noise that rounded hover rows make unnecessary.

## 3. Window structure

```
┌────────────┬──────────────────────────────────────────────┐
│ sidebar    │ unified toolbar (52 px)                      │
│ 220 px     │ [quick-add URL field] [Add] [Clear finished] │
│ #18181B    │            (toast)          [🔍 filter]      │
│            ├──────────────────────────────────────────────┤
│ brand      │ table header (NAME SIZE STATUS SPEED ADDED)  │
│ LIBRARY    │ rows: 56 px, radius 8, hover #FFFFFF08       │
│  All / Active / Completed / Failed   (with count badges)  │
│ GENERAL    │ [icon tile] name / meta line                 │
│  Add / Settings / About                [4 px progress]   │
│            │                            [hover actions]   │
│ STORAGE    │                                              │
│  x GB free ├──────────────────────────────────────────────┤
│  ▓▓▓▓░░░░  │ status bar (28 px): ↓ speed · mode · dir · db│
└────────────┴──────────────────────────────────────────────┘
```

* **Sidebar** — source-list style. Selected item = accent 15 % fill + white
  text; hover = `#FFFFFF08`; no left selection bar, no dividers. Two groups:
  LIBRARY (filtered views over the same table) and GENERAL (forms). The
  STORAGE widget is a card with a 4 px *linear* bar showing used fraction of
  the download volume (warning colour above 90 %), plus a queue pulse line
  (`n active · m queued` + current speed).
* **Unified toolbar** — the only header. Hosts the quick-add field (paste a
  URL, press Enter, done — the full Add form stays available for checksums
  and per-task options), the primary `Add` button, `Clear finished`, transient
  toast messages, and the name filter field (applied live, in Rust).
* **Table** — uppercase 11 px muted column header; divider-less rows with a
  category icon tile (derived from the file extension), two-line name cell
  (name + transport/segments/checksum meta), right-aligned size and speed,
  state label + 4 px linear progress, and row actions that sit at 32 %
  opacity until the row is hovered. Double-click opens a finished file.
* **Status bar** — 28 px, recessed, monochrome except the live speed.

## 4. Interaction model

| Task | Path |
|---|---|
| Add one URL | toolbar field → Enter |
| Add many URLs / checksum / paused | sidebar “Add Download” form (one URL per line) |
| Pause / resume / stop / retry / remove | row actions (hover), or CLI |
| Open file / containing folder | row actions; double-click = open file |
| Find a download | toolbar filter (substring, live) |
| Narrow the list | sidebar LIBRARY groups (All / Active / Completed / Failed) |
| Everything else | Settings form; persisted in redb |

State colours: downloading = accent, paused = warning, completed = success,
failed = danger (with the error text inline in the row's meta line). Unknown
size renders as a 22 % static fill (indeterminate affordance).

Feedback rules: every mutation produces either a visible state change or a
toast; errors are always toasts in danger colour; nothing modal except native
folder pickers.

## 5. Typography

Segoe UI Variable Text (Windows 11), 13 px base. Scale: 15/600 view titles →
13/500 row names → 12 body → 11 secondary → 10 meta/badges. Group captions
are 10 px, 600 weight, 1 px letter-spacing, uppercase. Emoji category glyphs
render via Segoe UI Emoji fallback.

## 6. Known gaps / roadmap

* Right-click context menus (popover style: radius 10, `#25252AE6`, 1 px
  `#FFFFFF22` border) — designed, not yet wired.
* File-type CATEGORIES sidebar group (needs an extension→category index in
  redb).
* Column sorting (needs an order key on the rows model).
* Light theme (tokens are ready; only `theme.slint` would change).
