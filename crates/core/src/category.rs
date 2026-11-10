//! Category routing (G2): classify a download by its sanitized filename and
//! route it into one subfolder of the download root.
//!
//! The rules here are deliberately pure and shared: the engine uses them to
//! lock a task's route, and adapters (GUI preview) use the same code so a
//! preview can never disagree with the engine. Nothing in this module looks
//! at MIME types, URL query strings or file contents — the filename is the
//! only input, exactly as the G2 contract requires.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// One routing rule: a category with its folder name and filename suffixes.
///
/// The folder name selects exactly one direct child of the download root and
/// must pass [`validate_folder_name`]; suffixes are matched case-insensitively
/// against the sanitized basename, longest suffix first.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CategoryRule {
    /// Stable category id (never localised, never renamed by language).
    pub id: String,
    /// Physical folder name under the download root (stable across EN/VI).
    pub folder: String,
    /// Filename suffixes including the dot, e.g. `.zip`, `.tar.gz`.
    #[serde(default)]
    pub suffixes: Vec<String>,
}

/// The built-in rule set from the G2 brief. `other` has no suffixes: it is
/// the fallthrough for names that match nothing, not a rule of its own.
pub fn default_categories() -> Vec<CategoryRule> {
    fn rule(id: &str, folder: &str, suffixes: &[&str]) -> CategoryRule {
        CategoryRule {
            id: id.to_string(),
            folder: folder.to_string(),
            suffixes: suffixes.iter().map(|s| s.to_string()).collect(),
        }
    }
    vec![
        rule(
            "documents",
            "Documents",
            &[
                ".pdf", ".doc", ".docx", ".odt", ".rtf", ".txt", ".md", ".xls", ".xlsx", ".ods",
                ".csv", ".ppt", ".pptx", ".odp", ".epub",
            ],
        ),
        rule(
            "music",
            "Music",
            &[
                ".mp3", ".flac", ".wav", ".ogg", ".oga", ".m4a", ".aac", ".opus", ".wma", ".aif",
                ".aiff", ".mid", ".midi",
            ],
        ),
        rule(
            "videos",
            "Videos",
            &[
                ".mp4", ".mkv", ".avi", ".mov", ".wmv", ".flv", ".webm", ".m4v", ".mpg", ".mpeg",
                ".3gp", ".ts",
            ],
        ),
        rule(
            "images",
            "Images",
            &[
                ".png", ".jpg", ".jpeg", ".gif", ".bmp", ".webp", ".svg", ".ico", ".tif", ".tiff",
                ".avif", ".heic", ".heif", ".raw",
            ],
        ),
        rule(
            "archives",
            "Archives",
            &[
                ".zip", ".tar", ".tar.gz", ".tgz", ".tar.bz2", ".tbz2", ".tar.xz", ".txz", ".gz",
                ".bz2", ".xz", ".7z", ".rar", ".zst", ".lz4",
            ],
        ),
        rule(
            "programs",
            "Programs",
            &[
                ".exe",
                ".msi",
                ".msix",
                ".msixbundle",
                ".appx",
                ".appxbundle",
                ".bat",
                ".cmd",
                ".ps1",
                ".pkg",
                ".deb",
                ".rpm",
                ".appimage",
                ".apk",
            ],
        ),
        rule(
            "disk-images",
            "Disk images",
            &[".iso", ".img", ".vhd", ".vhdx", ".vmdk", ".qcow2", ".dmg"],
        ),
        CategoryRule {
            id: "other".to_string(),
            folder: "Other".to_string(),
            suffixes: Vec::new(),
        },
    ]
}

/// Normalise a suffix for comparison: lowercase, exactly one leading dot.
fn normalize_suffix(raw: &str) -> Option<String> {
    let s = raw.trim().to_ascii_lowercase();
    if s.len() < 2 || !s.starts_with('.') || s.ends_with('.') {
        return None;
    }
    if s.chars().any(|c| c == '/' || c == '\\' || c.is_control()) {
        return None;
    }
    Some(s)
}

/// Validate a whole rule set the way the engine will use it.
///
/// Returns the set unchanged when valid. Duplicate suffixes (after
/// normalisation) across or within rules are rejected, as are duplicate
/// category ids and any folder name failing [`validate_folder_name`].
pub fn validate_categories(rules: &[CategoryRule]) -> Result<Vec<CategoryRule>> {
    let mut ids = std::collections::HashSet::new();
    let mut suffix_owner: BTreeMap<String, String> = BTreeMap::new();
    let mut out = Vec::with_capacity(rules.len());
    for rule in rules {
        let id = rule.id.trim();
        if id.is_empty() || !ids.insert(id.to_string()) {
            return Err(Error::Other(format!(
                "category id `{}` is empty or duplicated",
                rule.id
            )));
        }
        validate_folder_name(&rule.folder)
            .map_err(|e| Error::Other(format!("category `{id}` has an invalid folder: {e}")))?;
        let mut suffixes = Vec::with_capacity(rule.suffixes.len());
        for raw in &rule.suffixes {
            let suffix = normalize_suffix(raw).ok_or_else(|| {
                Error::Other(format!("category `{id}` has an invalid suffix `{raw}`"))
            })?;
            if let Some(owner) = suffix_owner.get(&suffix) {
                return Err(Error::Other(format!(
                    "suffix `{suffix}` is claimed by both `{owner}` and `{id}`"
                )));
            }
            suffix_owner.insert(suffix.clone(), id.to_string());
            suffixes.push(suffix);
        }
        out.push(CategoryRule {
            id: id.to_string(),
            folder: rule.folder.clone(),
            suffixes,
        });
    }
    Ok(out)
}

/// Classify a sanitized filename. Returns the matching category, or the
/// `other` category when nothing matches (including dotfiles and names
/// without a usable suffix). The match is on dotted suffixes of the
/// basename, case-insensitive, longest suffix first.
pub fn classify<'a>(filename: &str, rules: &'a [CategoryRule]) -> Option<&'a CategoryRule> {
    let lower = filename.to_ascii_lowercase();
    let mut best: Option<(&CategoryRule, usize)> = None;
    if !lower.starts_with('.') {
        for rule in rules {
            for suffix in &rule.suffixes {
                let suffix = suffix.to_ascii_lowercase();
                if lower.ends_with(&suffix) && lower.len() > suffix.len() {
                    let len = suffix.len();
                    if best.map(|(_, l)| len > l).unwrap_or(true) {
                        best = Some((rule, len));
                    }
                }
            }
        }
    }
    if let Some((rule, _)) = best {
        return Some(rule);
    }
    rules.iter().find(|r| r.id == "other")
}

/// Validate a category folder name: exactly one direct child folder.
///
/// Rejects (never sanitises) anything that could escape the root or collide
/// with Windows path semantics: separators, `.`/`..`, absolute or
/// drive-relative forms, UNC/device namespaces, colons (ADS), control
/// characters, device names with or without an extension, and trailing
/// dots/spaces.
pub fn validate_folder_name(name: &str) -> Result<String> {
    if name.is_empty() {
        return Err(Error::Other("category folder name is empty".into()));
    }
    if name != name.trim() || name.ends_with('.') || name.ends_with(' ') {
        return Err(Error::Other(format!(
            "category folder name `{name}` has leading/trailing space or a trailing dot"
        )));
    }
    if name == "." || name == ".." {
        return Err(Error::Other(format!(
            "category folder name `{name}` is not a folder name"
        )));
    }
    for c in name.chars() {
        match c {
            '/' | '\\' | ':' | '<' | '>' | '"' | '|' | '?' | '*' => {
                return Err(Error::Other(format!(
                    "category folder name `{name}` contains `{c}`"
                )));
            }
            _ if (c as u32) < 0x20 || (c as u32) == 0x7f => {
                return Err(Error::Other(format!(
                    "category folder name `{name}` contains a control character"
                )));
            }
            _ => {}
        }
    }
    // Windows device names are reserved with or without an extension.
    const RESERVED: [&str; 22] = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    let stem = name.split('.').next().unwrap_or("").to_ascii_uppercase();
    if RESERVED.contains(&stem.as_str()) {
        return Err(Error::Other(format!(
            "category folder name `{name}` is a reserved device name"
        )));
    }
    Ok(name.to_string())
}

// ---------------------------------------------------------------------------
// Path verification (contract section 6)
// ---------------------------------------------------------------------------

/// `true` when a path component is a symlink / junction / reparse point.
#[cfg(windows)]
fn is_reparse(path: &Path) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    match std::fs::symlink_metadata(path) {
        Ok(md) => {
            md.file_type().is_symlink()
                || (md.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT) != 0
        }
        Err(_) => false,
    }
}

/// On non-Windows builds only symlinks can redirect a path.
#[cfg(not(windows))]
fn is_reparse(path: &Path) -> bool {
    match std::fs::symlink_metadata(path) {
        Ok(md) => md.file_type().is_symlink(),
        Err(_) => false,
    }
}

/// Whether a canonical path names the same location as the path it came from.
///
/// Windows paths are case-insensitive: `C:\WINDOWS\Temp` canonicalizes to
/// `C:\Windows\Temp`, and a root the user typed as `c:\downloads` comes back
/// as `C:\Downloads`. A difference in letter case alone is not a redirection
/// (junctions and symlinks are caught component by component before this),
/// so it must not be refused. Any other difference, such as an 8.3 short name
/// expanding to its long name, still counts as a different spelling.
#[cfg(windows)]
fn same_spelling(real: &Path, given: &Path) -> bool {
    real == given || real.to_string_lossy().to_uppercase() == given.to_string_lossy().to_uppercase()
}

/// Elsewhere paths are case-sensitive and must match exactly.
#[cfg(not(windows))]
fn same_spelling(real: &Path, given: &Path) -> bool {
    real == given
}

/// The deepest existing ancestor of `dir` (or `dir` itself when it exists).
///
/// Used before any creation: the part of the path that already exists must
/// verify as plain *before* `create_dir_all` is allowed to add components
/// under it — otherwise a missing root beneath an existing junction would
/// be created outside the intended tree before the junction is rejected.
fn deepest_existing_ancestor(dir: &Path) -> PathBuf {
    let mut current = dir.to_path_buf();
    loop {
        if current.exists() {
            return current;
        }
        if !current.pop() {
            return current;
        }
    }
}

/// Verify that `dir` is a plain local directory whose every component from
/// the filesystem root down is free of reparse points, and that its real
/// (canonical) location is exactly where the components say it is.
pub fn verify_plain_dir(dir: &Path) -> Result<PathBuf> {
    if !dir.is_absolute() {
        return Err(Error::Route(format!(
            "download root {} is not absolute",
            crate::paths::display(dir)
        )));
    }
    let mut current = PathBuf::new();
    for component in dir.components() {
        match component {
            Component::RootDir => current.push(component.as_os_str()),
            Component::Prefix(prefix) => {
                use std::path::Prefix;
                match prefix.kind() {
                    Prefix::Disk(_) | Prefix::VerbatimDisk(_) => {
                        current.push(component.as_os_str())
                    }
                    _ => {
                        return Err(Error::Route(format!(
                            "download root {} uses a UNC or device namespace",
                            crate::paths::display(dir)
                        )))
                    }
                }
            }
            Component::Normal(name) => {
                current.push(name);
                if is_reparse(&current) {
                    return Err(Error::Route(format!(
                        "{} is a link or reparse point; category routing refuses it",
                        crate::paths::display(&current)
                    )));
                }
            }
            Component::CurDir | Component::ParentDir => {
                return Err(Error::Route(format!(
                    "download root {} contains a relative component",
                    crate::paths::display(dir)
                )))
            }
        }
    }
    let real = std::fs::canonicalize(dir).map_err(|e| {
        Error::Route(format!(
            "could not resolve download root {}: {e}",
            crate::paths::display(dir)
        ))
    })?;
    if !same_spelling(dunce::simplified(&real), dunce::simplified(dir)) {
        return Err(Error::Route(format!(
            "download root {} resolves to {}; category routing refuses redirected roots",
            crate::paths::display(dir),
            crate::paths::display(&real)
        )));
    }
    Ok(real)
}

/// Prepare the category destination directory for a locked route.
///
/// Verifies the root component-wise, creates the single child folder when
/// missing, refuses reparse points at the folder and at an existing
/// partial/final file, and re-verifies the folder after creation. Returns
/// the verified destination directory to use for file operations.
pub fn prepare_category_dir(root: &Path, folder: &str, filename: &str) -> Result<PathBuf> {
    // Every failure at this boundary is a terminal routing error carrying
    // its path and cause (Zed G2 re-review, P2): the folder validation and
    // every filesystem/canonicalization failure below are normalized into
    // `Error::Route` here, so nothing preparation-related can slip into the
    // worker's retry path as a bare `Other`/`Io`.
    validate_folder_name(folder).map_err(|e| {
        Error::Route(format!(
            "invalid category folder `{folder}` for download root {}: {e}",
            crate::paths::display(root)
        ))
    })?;
    if !root.is_absolute() {
        return Err(Error::Route(format!(
            "download root {} is not absolute",
            crate::paths::display(root)
        )));
    }
    // The root itself is the user's configured download folder: it may not
    // exist yet (the engine creates destination folders on demand). Before
    // ANY creation, the deepest existing ancestor must verify as plain —
    // creating first and validating later would let a missing root beneath
    // an existing junction materialise directories outside the intended
    // tree before the junction is rejected (Zed G2 review, P1). After
    // creation the full root verifies again.
    verify_plain_dir(&deepest_existing_ancestor(root))?;
    if !root.exists() {
        match std::fs::create_dir_all(root) {
            Ok(()) => {}
            // Another task may have created the root concurrently; the
            // full verification right below decides whether what exists is
            // acceptable — AlreadyExists is not a failure on its own.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => {
                return Err(Error::Route(format!(
                    "could not create download root {}: {e}",
                    crate::paths::display(root)
                )))
            }
        }
    }
    verify_plain_dir(root)?;
    let dest = root.join(folder);
    if is_reparse(&dest) {
        return Err(Error::Route(format!(
            "{} is a link or reparse point; category routing refuses it",
            crate::paths::display(&dest)
        )));
    }
    if !dest.exists() {
        match std::fs::create_dir(&dest) {
            Ok(()) => {}
            // A concurrent task may have won creation of the same category
            // folder; fall through to the existence/type/reparse checks
            // below and the final verification instead of failing outright.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => {
                return Err(Error::Route(format!(
                    "could not create category folder {}: {e}",
                    crate::paths::display(&dest)
                )))
            }
        }
    }
    if !dest.is_dir() {
        return Err(Error::Route(format!(
            "{} exists and is not a folder",
            crate::paths::display(&dest)
        )));
    }
    // Re-verify after creation: the folder that exists now must still be a
    // plain folder directly under the verified root.
    verify_plain_dir(&dest)?;
    for candidate in [
        dest.join(filename),
        dest.join(crate::util::part_filename(filename)),
    ] {
        if is_reparse(&candidate) {
            return Err(Error::Route(format!(
                "{} is a link or reparse point; category routing refuses it",
                crate::paths::display(&candidate)
            )));
        }
    }
    Ok(dest)
}

/// Provisional destination for display only (Add-form preview). Uses the
/// same classifier as the engine; the result is *not* a promise — the
/// final filename is only known after the probe. Creates nothing.
pub fn preview_dest(
    root: &Path,
    url_filename: Option<&str>,
    rules: &[CategoryRule],
) -> Option<PathBuf> {
    let name = url_filename?;
    let rule = classify(name, rules)?;
    Some(root.join(&rule.folder).join(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules() -> Vec<CategoryRule> {
        validate_categories(&default_categories()).unwrap()
    }

    #[test]
    fn classifies_by_longest_suffix_case_insensitively() {
        let r = rules();
        assert_eq!(classify("backup.TAR.GZ", &r).unwrap().id, "archives");
        assert_eq!(classify("song.Mp3", &r).unwrap().id, "music");
        assert_eq!(classify("movie.mkv", &r).unwrap().id, "videos");
        assert_eq!(classify("paper.PDF", &r).unwrap().id, "documents");
        assert_eq!(classify("photo.heic", &r).unwrap().id, "images");
        assert_eq!(classify("setup.msi", &r).unwrap().id, "programs");
        assert_eq!(classify("ubuntu.iso", &r).unwrap().id, "disk-images");
    }

    #[test]
    fn dotfiles_and_unknown_names_go_to_other() {
        let r = rules();
        assert_eq!(classify(".tar.gz", &r).unwrap().id, "other");
        assert_eq!(classify("README", &r).unwrap().id, "other");
        assert_eq!(classify("data.parquet", &r).unwrap().id, "other");
        assert_eq!(classify("trailing.", &r).unwrap().id, "other");
    }

    #[test]
    fn duplicate_suffixes_are_rejected() {
        let mut r = default_categories();
        r.push(CategoryRule {
            id: "extra".into(),
            folder: "Extra".into(),
            suffixes: vec![".ZIP".into()],
        });
        assert!(validate_categories(&r).is_err());
    }

    #[test]
    fn folder_names_are_validated_strictly() {
        assert!(validate_folder_name("Videos").is_ok());
        assert!(validate_folder_name("Disk images").is_ok());
        for bad in [
            "", " ", "..", ".", "a/b", "a\\b", "C:foo", "foo:bar", "CON", "con.txt", "LPT1",
            "name.", "name ", " lead", "a|b", "a?b", "a*b", "a<b", "a\"b",
        ] {
            assert!(validate_folder_name(bad).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn preview_uses_the_same_classifier() {
        let r = rules();
        let root = Path::new("C:\\Downloads");
        let p = preview_dest(root, Some("film.mkv"), &r).unwrap();
        assert_eq!(p, root.join("Videos").join("film.mkv"));
        assert!(preview_dest(root, None, &r).is_none());
    }

    #[test]
    fn missing_root_is_created_under_a_plain_ancestor() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("a").join("b").join("downloads");
        let dest = prepare_category_dir(&root, "Music", "x.mp3").unwrap();
        assert_eq!(dest, dunce::simplified(&root).join("Music"));
        assert!(dest.is_dir());
    }

    #[cfg(windows)]
    #[test]
    fn missing_root_beneath_a_junction_is_refused_before_creation() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tmp.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        let link = tmp.path().join("link");
        let status = std::process::Command::new("cmd")
            .args([
                "/C",
                "mklink",
                "/J",
                &link.to_string_lossy(),
                &outside.to_string_lossy(),
            ])
            .status()
            .unwrap();
        assert!(status.success(), "mklink /J failed");
        // The configured root does not exist yet and sits *beneath* the
        // junction: creation must be refused before anything appears
        // outside the intended tree.
        let root = link.join("downloads");
        let err = prepare_category_dir(&root, "Music", "x.mp3").unwrap_err();
        assert!(matches!(err, Error::Route(_)), "unexpected error: {err:?}");
        assert!(!outside.join("downloads").exists());
    }

    #[test]
    fn preparation_failures_are_terminal_route_errors() {
        // Zed G2 re-review, P2: nothing at the preparation boundary may
        // surface as a retryable `Other`/`Io` — validation, canonicalize
        // and create failures all normalize to `Error::Route`.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("downloads");
        // Invalid folder name (validation used to return `Error::Other`).
        let err = prepare_category_dir(&root, "a/b", "x.mp3").unwrap_err();
        assert!(matches!(err, Error::Route(_)), "folder validation: {err:?}");
        assert!(err.is_fatal(), "route errors must be fatal: {err:?}");
        // Root collides with an existing *file*: canonicalize/creation
        // fails, and that failure is a routing error too.
        let file_root = tmp.path().join("not-a-dir");
        std::fs::write(&file_root, b"x").unwrap();
        let err = prepare_category_dir(&file_root, "Music", "x.mp3").unwrap_err();
        assert!(matches!(err, Error::Route(_)), "file-as-root: {err:?}");
    }

    #[cfg(windows)]
    #[test]
    fn a_root_spelled_in_different_letter_case_is_accepted() {
        // The runner's SYSTEM temp folder is C:\WINDOWS\SystemTemp, which
        // canonicalizes to C:\Windows\SystemTemp; users can type any case too.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("Downloads");
        std::fs::create_dir(&root).unwrap();
        let shouted = PathBuf::from(root.to_string_lossy().to_uppercase());
        assert!(verify_plain_dir(&shouted).is_ok());
        let dest = prepare_category_dir(&shouted, "Music", "x.mp3").unwrap();
        assert!(dest.is_dir());
    }

    #[test]
    fn plain_dir_verification_accepts_a_real_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        std::fs::create_dir(&root).unwrap();
        let dest = prepare_category_dir(&root, "Videos", "a.mkv").unwrap();
        assert_eq!(dest, dunce::simplified(&root).join("Videos"));
        assert!(dest.is_dir());
        // Second call reuses the folder.
        let dest2 = prepare_category_dir(&root, "Videos", "a.mkv").unwrap();
        assert_eq!(dest, dest2);
    }
}
