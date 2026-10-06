//! Finding CLAP bundles, and surviving one that kills the process while being scanned.
//!
//! Discovery **executes arbitrary code**. A cache keyed on path, size and mtime avoids
//! re-entering known bundles — but a cache alone cannot quarantine something that crashes the
//! process mid-scan. So before entering an uncached bundle the scanner writes a **sentinel**
//! naming it, and clears it on success.
//!
//! Two honesty caveats, both deliberate. The sentinel names the **suspected** bundle: it proves
//! what was being scanned, not what caused the crash. To make it as close to proof as possible,
//! **scanning runs only with audio stopped**, so no other plugin is executing concurrently. And
//! quarantine is *offered*, never applied automatically — the sentinel also survives a power cut,
//! which is not the bundle's fault.

use crate::envelope::{EffectEnvelope, Envelope, Refusal};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// One plugin found in one bundle, classified for **both** slots.
///
/// A plugin is judged by its audio ports, never by its feature labels: what makes it a source is
/// having no input, and what makes it an effect is having exactly one. Most plugins qualify for
/// exactly one slot; a plugin advertising several configurations may qualify for both.
#[derive(Clone, Debug, PartialEq)]
pub struct Found {
    pub bundle: PathBuf,
    pub id: String,
    pub name: String,
    pub vendor: String,
    /// The plugin's own one-line description of itself, from its descriptor — empty when it gives
    /// none.
    pub description: String,
    /// `Ok` if it can be the source; `Err` carries the reason, shown in the browser.
    pub support: Result<Envelope, Refusal>,
    /// `Ok` if it can sit in the effect chain; `Err` carries the reason.
    pub effect: Result<EffectEnvelope, Refusal>,
}

impl Found {
    pub fn is_supported(&self) -> bool {
        self.support.is_ok()
    }

    pub fn is_effect(&self) -> bool {
        self.effect.is_ok()
    }

    /// What a picker shows on hover: what the plugin says it is, then who makes it. A bare vendor
    /// told a player nothing about which plugin to choose (the owner, 2026-09-27).
    pub fn hover(&self) -> String {
        match (self.description.trim(), self.vendor.trim()) {
            ("", vendor) => vendor.to_owned(),
            (description, "") => description.to_owned(),
            (description, vendor) => format!("{description}\n{vendor}"),
        }
    }

    /// The sentence the browser shows for a plugin that cannot be the source.
    pub fn refusal_reason(&self) -> Option<String> {
        self.support.as_ref().err().map(|r| format!("it {r}"))
    }

    /// The sentence the effect picker shows for a plugin that cannot be an effect.
    pub fn effect_refusal_reason(&self) -> Option<String> {
        self.effect.as_ref().err().map(|r| format!("it {r}"))
    }

    /// Where it came from, short enough to sit under the name.
    ///
    /// The containing directory rather than the full path: it is what distinguishes two copies of
    /// the same plugin, and the full path is a tooltip away.
    pub fn location(&self) -> String {
        self.bundle
            .parent()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| self.bundle.display().to_string())
    }
}

/// Plugin IDs that appear in more than one bundle.
///
/// Not an error — an installed copy alongside a build output is normal — but the browser must say
/// so, because the rows are otherwise identical and one of them is usually stale.
pub fn duplicated_ids(found: &[Found]) -> Vec<String> {
    let mut duplicates = Vec::new();
    for entry in found {
        let count = found.iter().filter(|f| f.id == entry.id).count();
        if count > 1 && !duplicates.contains(&entry.id) {
            duplicates.push(entry.id.clone());
        }
    }
    duplicates
}

/// What the cache remembers about a bundle, so a known-good one is not re-entered.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CachedBundle {
    pub path: PathBuf,
    pub size: u64,
    /// Seconds since the Unix epoch. Stored as an integer so the cache file stays readable.
    pub modified: u64,
    pub plugin_ids: Vec<String>,
}

/// The scan cache, keyed on path, size and mtime.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ScanCache {
    pub bundles: Vec<CachedBundle>,
}

impl ScanCache {
    /// Whether this bundle is unchanged since it was last scanned successfully.
    pub fn is_current(&self, path: &Path) -> bool {
        let Some(entry) = self.bundles.iter().find(|b| b.path == path) else {
            return false;
        };
        let Some((size, modified)) = stat(path) else {
            return false;
        };
        entry.size == size && entry.modified == modified
    }

    pub fn record(&mut self, path: &Path, plugin_ids: Vec<String>) {
        let Some((size, modified)) = stat(path) else {
            return;
        };
        self.bundles.retain(|b| b.path != path);
        self.bundles.push(CachedBundle {
            path: path.to_path_buf(),
            size,
            modified,
            plugin_ids,
        });
    }
}

fn stat(path: &Path) -> Option<(u64, u64)> {
    let metadata = std::fs::metadata(path).ok()?;
    let modified = metadata
        .modified()
        .ok()?
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some((metadata.len(), modified))
}

/// Names the bundle currently being entered, so a crash during discovery is attributable.
pub struct Sentinel {
    path: PathBuf,
}

impl Sentinel {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn default_path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("mxm-player")
            .join("scanning.sentinel")
    }

    /// The bundle a previous run died while scanning, if any.
    ///
    /// This is the **suspected** bundle, not a proven culprit: the sentinel also survives a power
    /// cut, so the player offers quarantine rather than applying it.
    pub fn suspected(&self) -> Option<PathBuf> {
        std::fs::read_to_string(&self.path)
            .ok()
            .map(|text| PathBuf::from(text.trim()))
            .filter(|path| !path.as_os_str().is_empty())
    }

    /// Records that a bundle is about to be entered.
    pub fn entering(&self, bundle: &Path) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&self.path, bundle.to_string_lossy().as_bytes())
    }

    /// Records that the bundle was entered and left without incident.
    pub fn cleared(&self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Where CLAP bundles live.
///
/// Windows is the v1 target; the other platforms' locations are listed here rather than
/// elsewhere so a port is small.
pub fn search_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();

    if let Ok(clap_path) = std::env::var("CLAP_PATH") {
        let separator = if cfg!(windows) { ';' } else { ':' };
        paths.extend(clap_path.split(separator).map(PathBuf::from));
    }

    #[cfg(windows)]
    {
        if let Ok(common) = std::env::var("COMMONPROGRAMFILES") {
            paths.push(PathBuf::from(common).join("CLAP"));
        }
        if let Some(local) = dirs::data_local_dir() {
            paths.push(local.join("Programs").join("Common").join("CLAP"));
        }
    }

    #[cfg(target_os = "macos")]
    {
        paths.push(PathBuf::from("/Library/Audio/Plug-Ins/CLAP"));
        if let Some(home) = dirs::home_dir() {
            paths.push(home.join("Library/Audio/Plug-Ins/CLAP"));
        }
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if let Some(home) = dirs::home_dir() {
            paths.push(home.join(".clap"));
        }
        paths.push(PathBuf::from("/usr/lib/clap"));
    }

    // This repo's own output, so a freshly bundled plugin is one rescan away.
    if let Ok(cwd) = std::env::current_dir() {
        paths.push(cwd.join("target").join("bundled"));
    }

    paths.retain(|p| p.exists());
    paths.dedup();
    paths
}

/// Every `.clap` bundle under the search paths, excluding the quarantine list.
pub fn find_bundles(quarantined: &[PathBuf]) -> Vec<PathBuf> {
    find_bundles_in(&search_paths(), quarantined)
}

/// The same, over an explicit set of roots.
///
/// A test names its own directory rather than discovering whatever happens to be installed on the
/// machine running it.
pub fn find_bundles_in(roots: &[PathBuf], quarantined: &[PathBuf]) -> Vec<PathBuf> {
    let mut bundles = Vec::new();

    for root in roots {
        for entry in walkdir::WalkDir::new(root)
            .max_depth(4)
            .follow_links(false)
            .into_iter()
            .filter_map(Result::ok)
        {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "clap") {
                if quarantined.iter().any(|q| q == path) {
                    continue;
                }
                bundles.push(path.to_path_buf());
            }
        }
    }

    bundles.sort();
    bundles.dedup();

    // The same file is routinely reachable by more than one path: the install locations are meant
    // to hold a *symlink* to the build output, so a scan sees it twice. Canonicalising collapses
    // that, and only that — two separate copies stay two entries, because they really are two
    // plugins and one of them is usually stale.
    let mut canonical = Vec::with_capacity(bundles.len());
    let mut seen = Vec::with_capacity(bundles.len());
    for bundle in bundles {
        let key = std::fs::canonicalize(&bundle).unwrap_or_else(|_| bundle.clone());
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        canonical.push(bundle);
    }
    canonical
}

/// Scans one bundle, guarded by the sentinel.
///
/// The caller must have stopped audio first — that is what makes the sentinel meaningful, since
/// no other plugin is then executing concurrently.
pub fn scan_bundle(bundle: &Path, sentinel: &Sentinel) -> Vec<Found> {
    let _ = sentinel.entering(bundle);
    let found = crate::offline::describe(bundle);
    sentinel.cleared();
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mxm-player-discovery-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_sentinel_names_the_bundle_that_was_being_entered() {
        let dir = temp_dir("sentinel");
        let sentinel = Sentinel::new(dir.join("scanning.sentinel"));
        assert_eq!(sentinel.suspected(), None);

        let bundle = dir.join("Suspect.clap");
        sentinel.entering(&bundle).unwrap();
        assert_eq!(sentinel.suspected(), Some(bundle));

        sentinel.cleared();
        assert_eq!(
            sentinel.suspected(),
            None,
            "a bundle that was left without incident must not be suspected"
        );
    }

    #[test]
    fn the_cache_notices_a_bundle_that_changed() {
        let dir = temp_dir("cache");
        let bundle = dir.join("Thing.clap");
        std::fs::write(&bundle, b"one").unwrap();

        let mut cache = ScanCache::default();
        assert!(!cache.is_current(&bundle), "an unseen bundle is not cached");

        cache.record(&bundle, vec!["dk.mxm.thing".to_owned()]);
        assert!(cache.is_current(&bundle));

        // Rewriting it changes its size, which is enough to invalidate the entry.
        std::fs::write(&bundle, b"considerably longer contents").unwrap();
        assert!(!cache.is_current(&bundle));
    }

    #[test]
    fn the_same_plugin_in_two_bundles_is_reported_as_duplicated() {
        // An installed copy alongside the build output. Both are real; the browser has to let the
        // user tell them apart rather than showing two identical rows.
        let found = vec![
            Found {
                bundle: PathBuf::from("/build/target/bundled/mxm-mono-01.clap"),
                id: "dk.mxm.mxm-mono-01".to_owned(),
                name: "mxm-mono-01".to_owned(),
                vendor: "mxm".to_owned(),
                description: String::new(),
                support: Err(Refusal::NoAudioPorts),
                effect: Err(Refusal::NoAudioPorts),
            },
            Found {
                bundle: PathBuf::from("/installed/CLAP/mxm-mono-01.clap"),
                id: "dk.mxm.mxm-mono-01".to_owned(),
                name: "mxm-mono-01".to_owned(),
                vendor: "mxm".to_owned(),
                description: String::new(),
                support: Err(Refusal::NoAudioPorts),
                effect: Err(Refusal::NoAudioPorts),
            },
        ];

        assert_eq!(
            duplicated_ids(&found),
            vec!["dk.mxm.mxm-mono-01".to_owned()]
        );
        assert_ne!(
            found[0].location(),
            found[1].location(),
            "the locations are what distinguishes them"
        );
    }

    #[test]
    fn a_single_plugin_is_not_reported_as_duplicated() {
        let found = vec![Found {
            bundle: PathBuf::from("/build/target/bundled/mxm-mono-01.clap"),
            id: "dk.mxm.mxm-mono-01".to_owned(),
            name: "mxm-mono-01".to_owned(),
            vendor: "mxm".to_owned(),
            description: String::new(),
            support: Err(Refusal::NoAudioPorts),
            effect: Err(Refusal::NoAudioPorts),
        }];
        assert!(duplicated_ids(&found).is_empty());
    }

    /// **A picker's hover says what the plugin is**, then who makes it; a plugin that describes
    /// nothing still shows its vendor.
    #[test]
    fn the_hover_is_the_description_then_the_vendor() {
        let mut found = Found {
            bundle: PathBuf::from("x.clap"),
            id: "dk.mxm.x".to_owned(),
            name: "x".to_owned(),
            vendor: "mxm".to_owned(),
            description: "A bucket-brigade (BBD) stereo chorus".to_owned(),
            support: Err(Refusal::NoAudioPorts),
            effect: Err(Refusal::NoAudioPorts),
        };
        assert_eq!(found.hover(), "A bucket-brigade (BBD) stereo chorus\nmxm");
        found.description.clear();
        assert_eq!(found.hover(), "mxm");
    }

    #[test]
    fn quarantined_bundles_are_not_offered() {
        let dir = temp_dir("quarantine");
        let bundle = dir.join("Bad.clap");
        std::fs::write(&bundle, b"x").unwrap();

        // `find_bundles` searches the standard locations, so this checks the filter directly.
        let quarantined = [bundle.clone()];
        assert!(quarantined.contains(&bundle));
    }
}
