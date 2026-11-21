# cino-ui snapshot used by UDM

Source: user-provided `slint-ui` library, cino-ui0.1.0, copied2026-11-04.
The source folder had no Git repository, so no upstream commit is claimed.
MIT license: LICENSE. Be Vietnam Pro fonts: ui/fonts/OFL.txt.

UDM uses the real Theme, Lang, AppearanceGroup and TextField components through
`@cino-ui`. The existing UDM token names are an adapter over the shared Theme;
task UI, table geometry, persisted preferences and download strings stay in UDM.
No path to a sibling checkout is required to build or run the candidate.

The source library targets Slint1.18. UDM currently targets1.6. index-compat.slint
imports only the required UI modules and uses1.6 re-export syntax; original
ui/index.slint remains unchanged. This avoids pulling in a second Slint runtime,
frameless window loop, tray or Windows/registry helpers. The source library
outside this repository is untouched.

Local API extension: AppearanceGroup exposes an optional `accents` model passed
to AccentPicker. UDM supplies five contrast-safe fills (blue/green/orange/pink/
purple), maintaining the shared Theme's two inputs: dark and accent. This small
extension can be contributed upstream after independent review. TextField also
constrains its placeholder to the field width and explicitly fills the input
height, preserving UDM's aligned inputs with long EN/VI strings. Swatch uses the
theme label for its selected ring so it remains visible on the light group.

Local icon extension (UDM 0.3.1): `ui/icons.slint` is a local extension of the
snapshot, not an untouched upstream file. The 14 baseline glyphs from `sliders`
through `minus` keep their original initializers. `IconData` gains an additive
`parts: [IconPart]` field (`IconPart` is stroke, fill, and ink). When `parts` is
empty, `Icon` still draws the legacy stroke in `Icon.color` and then the
optional fill. When `parts` is nonempty, those two paths are suppressed and the
parts paint back to front in their own inks. `Icon.color` does not tint parts.
`Icon.enabled` fades the composed icon (opacity 0.45 when false); there is no
second disabled set.

`IconPalette` is a local read-only global. Its tokens follow `Theme.dark` and
introduce no new Theme input. `Theme.accent` and `ui/theme.slint` are unchanged.
Hex literals for these inks live only in that global. Colored local entries
leave the legacy stroke and fill empty.

The local set after `minus` is chevron-down, download, play, pause, stop, trash,
file, folder, link, clear, library, grid, all-downloads, unfinished,
active-downloads, failed, documents, music, videos, images, archives, programs,
disk-images, other, all-categories, custom-category, chrome, and firefox. Chrome
and Firefox are original multicolored Path pictograms in the same geometry, not
brand logos and not copied logo paths. Colored variants of borrowed baseline
slots are appended as `udm-sliders`, `udm-info`, `udm-check`, `udm-refresh`,
`udm-chevron-right`, and `udm-plus`. Call sites choose them explicitly. Finished
and Completed share `udm-check`; Unfinished keeps its own pending glyph. `close`
has no `udm-` variant: it is not on an in-scope 0.3.1 surface.

`index-compat.slint` re-exports `Icon`, `Icons`, `IconData`, `IconPart`,
`IconPalette`, and `Gallery` with the existing 1.6 syntax. `ui/gallery.slint` is
a local evidence sheet, not a shipping surface: the 14 baseline cells, the
recolored local cells, the `udm-` variants, disabled examples, and swatches of
the window, sidebar, hover, and selected surfaces. `ui/index.slint` stays off
the build path. `crates/gui/ui/icon_gallery_capture.slint` is a development-only
window that imports `Gallery` and `Theme` through `@cino-ui` and sets
`Theme.dark`; the shipping window does not show it.

`upstream-files.sha256` is unchanged. It remains the baseline inventory of the
snapshot digests at integration time. Other copied UI/font files remain upstream
originals.

Only this compatibility entry is mapped by build.rs. Future upstream updates must
be reviewed and tested against both UDM Slint compile paths; do not overwrite
the snapshot blindly or silently enable the Rust Windows helpers.
