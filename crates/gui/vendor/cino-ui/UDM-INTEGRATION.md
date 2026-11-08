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
Other copied
UI/font files remain upstream originals; upstream-files.sha256 records the source
files' digests at integration time for future updates.

Only this compatibility entry is mapped by build.rs. Future upstream updates must
be reviewed and tested against both UDM Slint compile paths; do not overwrite
the snapshot blindly or silently enable the Rust Windows helpers.
