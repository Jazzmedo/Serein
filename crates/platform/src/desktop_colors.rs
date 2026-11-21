//! Linux desktop colors, followed live off the UI thread: the Settings portal's accent, with
//! GNOME, KDE and pywal fallbacks, and a `theme.json` that desktop palette tools (matugen,
//! Noctalia, Caelestia, pywal templates) rewrite when the scheme changes.
//!
//! The portal `accent-color` key is documented by the freedesktop Settings portal spec. The
//! fallback files are unofficial: they are read the way those desktops currently store them.

use crate::system_theme::{
	COMMAND_TIMEOUT, PORTAL_TIMEOUT, bounded_output, follow_theme_changes, portal_settings,
	reconnect_portal, unquote,
};
use std::{
	collections::BTreeMap,
	ffi::OsString,
	path::{Path, PathBuf},
	sync::{
		Arc, Mutex, PoisonError,
		atomic::{AtomicBool, AtomicU32, Ordering},
	},
	time::{Duration, SystemTime},
};

/// Theme-file colors by palette field name, as `#rrggbb` or `#rrggbbaa` text.
pub type ThemeColors = BTreeMap<String, String>;

/// How often the theme file and the fallback accent files are checked.
const POLL: Duration = Duration::from_secs(2);
/// Settings files are a few kilobytes; anything larger is not one of them.
const MAX_FILE: u64 = 256 * 1024;

#[derive(Default)]
struct Shared {
	enabled: AtomicBool,
	changed: tokio::sync::Notify,
	/// Packed as `0x01rrggbb`; 0 is "no accent", so pure black still counts as present.
	portal_accent: AtomicU32,
	fallback_accent: AtomicU32,
	/// A generation that changes with every new file value, so the UI re-applies only then.
	file: Mutex<(u64, Option<Arc<ThemeColors>>)>,
}

/// Latest desktop accent and theme-file colors.
pub struct DesktopColors {
	shared: Arc<Shared>,
	tasks: [tokio::task::JoinHandle<()>; 2],
}

impl DesktopColors {
	/// Starts following the desktop and calls `wake` whenever a color changes. The file and
	/// fallback polling stay idle until [`Self::set_enabled`] turns them on.
	pub fn watch(
		runtime: &tokio::runtime::Runtime,
		wake: impl Fn() + Send + Sync + 'static,
	) -> Self {
		let shared = Arc::new(Shared::default());
		let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(wake);
		let portal = runtime.spawn({
			let (shared, wake) = (shared.clone(), wake.clone());
			async move {
				let publish = |accent| store(&shared.portal_accent, accent, &*wake);
				reconnect_portal(
					|| async {
						let followed = portal(&publish).await;
						// The portal no longer answers: hand the accent to the fallback files.
						publish(None);
						followed
					},
					|| std::future::ready(()),
				)
				.await;
			}
		});
		let files = runtime.spawn({
			let shared = shared.clone();
			async move {
				let path = theme_path(&|name| std::env::var_os(name));
				let mut state = FileState::default();
				loop {
					if shared.enabled.load(Ordering::Relaxed) {
						let portal_answers = shared.portal_accent.load(Ordering::Relaxed) != 0;
						let path = path.clone();
						let Ok((fallback, file, next)) = tokio::task::spawn_blocking(move || {
							let env = |name: &str| std::env::var_os(name);
							// While the portal has an accent it wins; the files stay unread.
							let fallback =
								(!portal_answers).then(|| fallback_accent(&env)).flatten();
							let file = path.as_deref().and_then(|path| poll_file(path, &mut state));
							(fallback, file, state)
						})
						.await
						else {
							return;
						};
						state = next;
						store(&shared.fallback_accent, fallback, &*wake);
						if let Some(file) = file {
							set_file(&shared, file.map(Arc::new));
							wake();
						}
					} else {
						// Re-enabled, the file is read again at once instead of trusting old state.
						state = FileState::default();
						set_file(&shared, None);
					}
					let _ = tokio::time::timeout(POLL, shared.changed.notified()).await;
				}
			}
		});
		Self {
			shared,
			tasks: [portal, files],
		}
	}

	/// The Themes page's desktop-colors switch; off stops reading the desktop's files.
	pub fn set_enabled(&self, enabled: bool) {
		if self.shared.enabled.swap(enabled, Ordering::Relaxed) != enabled {
			self.shared.changed.notify_one();
		}
	}

	/// The desktop accent: the portal's while it answers, otherwise the fallback files'.
	pub fn accent(&self) -> Option<[u8; 3]> {
		unpack(self.shared.portal_accent.load(Ordering::Relaxed))
			.or_else(|| unpack(self.shared.fallback_accent.load(Ordering::Relaxed)))
	}

	/// The theme file's colors and the generation they were read in.
	pub fn file(&self) -> (u64, Option<Arc<ThemeColors>>) {
		self.shared
			.file
			.lock()
			.unwrap_or_else(PoisonError::into_inner)
			.clone()
	}
}

impl Drop for DesktopColors {
	fn drop(&mut self) {
		for task in &self.tasks {
			task.abort();
		}
	}
}

fn store(slot: &AtomicU32, accent: Option<[u8; 3]>, wake: &dyn Fn()) {
	let packed = accent.map_or(0, |[r, g, b]| u32::from_be_bytes([1, r, g, b]));
	if slot.swap(packed, Ordering::Relaxed) != packed {
		wake();
	}
}

fn unpack(packed: u32) -> Option<[u8; 3]> {
	let [present, r, g, b] = packed.to_be_bytes();
	(present != 0).then_some([r, g, b])
}

fn set_file(shared: &Shared, colors: Option<Arc<ThemeColors>>) {
	let mut file = shared.file.lock().unwrap_or_else(PoisonError::into_inner);
	if file.1.is_some() || colors.is_some() {
		*file = (file.0 + 1, colors);
	}
}

/// Follows the portal's `accent-color`. A failed read is "no accent", not the end of the
/// subscription, so a portal that gains an accent later is still followed.
async fn portal(publish: &impl Fn(Option<[u8; 3]>)) -> bool {
	use tokio::time::timeout;
	let Some(settings) = portal_settings().await else {
		return false;
	};
	let Ok(Ok(changes)) = timeout(PORTAL_TIMEOUT, settings.receive_accent_color_changed()).await
	else {
		return false;
	};
	follow_theme_changes(
		changes,
		|| async {
			let color = timeout(PORTAL_TIMEOUT, settings.accent_color()).await;
			Some(
				color
					.ok()
					.and_then(Result::ok)
					.and_then(|c| accent_from_floats(c.red(), c.green(), c.blue())),
			)
		},
		publish,
	)
	.await
}

/// The portal's 0..1 channels; the spec reserves out-of-range values for "unset".
fn accent_from_floats(r: f64, g: f64, b: f64) -> Option<[u8; 3]> {
	let channel = |v: f64| (0.0..=1.0).contains(&v).then(|| (v * 255.0).round() as u8);
	Some([channel(r)?, channel(g)?, channel(b)?])
}

/// `#rrggbb`, `rrggbb`, KDE's `r,g,b` byte triple, and `(r, g, b)` tuples some portals
/// stringify, as 0..1 floats or bytes.
fn parse_accent_text(text: &str) -> Option<[u8; 3]> {
	let text = text.trim();
	if let Some(tuple) = text.strip_prefix('(').and_then(|t| t.strip_suffix(')')) {
		let mut parts = tuple.split(',').map(|p| p.trim().parse::<f64>().ok());
		let (r, g, b) = (parts.next()??, parts.next()??, parts.next()??);
		if r <= 1.0 && g <= 1.0 && b <= 1.0 {
			return accent_from_floats(r, g, b);
		}
		let byte = |v: f64| (0.0..=255.0).contains(&v).then_some(v as u8);
		return Some([byte(r)?, byte(g)?, byte(b)?]);
	}
	if text.contains(',') {
		let mut parts = text.split(',').map(|p| p.trim().parse::<u8>().ok());
		return Some([parts.next()??, parts.next()??, parts.next()??]);
	}
	let hex = text.strip_prefix('#').unwrap_or(text);
	if hex.len() != 6 {
		return None;
	}
	let value = u32::from_str_radix(hex, 16).ok()?;
	let [_, r, g, b] = value.to_be_bytes();
	Some([r, g, b])
}

/// GNOME's named accents; the empty string is its default blue.
fn gnome_named_accent(name: &str) -> Option<[u8; 3]> {
	Some(match name {
		"" | "blue" => [0x35, 0x84, 0xe4],
		"teal" => [0x21, 0x90, 0xa4],
		"green" => [0x3a, 0x94, 0x4a],
		"yellow" => [0xc8, 0x88, 0x00],
		"orange" => [0xed, 0x5b, 0x00],
		"red" => [0xe6, 0x2d, 0x42],
		"pink" => [0xd5, 0x61, 0x99],
		"purple" => [0x91, 0x41, 0xac],
		"slate" => [0x6f, 0x83, 0x96],
		_ => return None,
	})
}

type Env<'a> = &'a dyn Fn(&str) -> Option<OsString>;

/// GNOME's named accent, then KDE's `kdeglobals`, then pywal's palette.
fn fallback_accent(var: Env) -> Option<[u8; 3]> {
	gnome_accent(var)
		.or_else(|| kde_accent(var))
		.or_else(|| wal_accent(var))
}

/// Only in a GNOME session: `gsettings` ships with GLib everywhere and elsewhere answers the
/// same empty "default", which would hide the desktop's real accent behind GNOME's blue.
fn gnome_accent(var: Env) -> Option<[u8; 3]> {
	if !gnome_session(var) {
		return None;
	}
	let output = bounded_output(
		std::process::Command::new("gsettings").args([
			"get",
			"org.gnome.desktop.interface",
			"accent-color",
		]),
		COMMAND_TIMEOUT,
	)?;
	gnome_named_accent(unquote(&output))
}

fn gnome_session(var: Env) -> bool {
	var("XDG_CURRENT_DESKTOP").is_some_and(|desktop| {
		desktop
			.to_string_lossy()
			.to_ascii_uppercase()
			.contains("GNOME")
	})
}

/// `AccentColor`, falling back to the color scheme's selection background.
fn kde_accent(var: Env) -> Option<[u8; 3]> {
	let text = read_text(&config_dir(var)?.join("kdeglobals"))?;
	parse_accent_text(
		&ini_value(&text, "General", "AccentColor")
			.or_else(|| ini_value(&text, "Colors:View", "SelectionBackground"))?,
	)
}

/// pywal's `color4`, the accent its templates use.
fn wal_accent(var: Env) -> Option<[u8; 3]> {
	let home = var("HOME")
		.map(PathBuf::from)
		.filter(|home| home.is_absolute())?;
	let text = read_text(&home.join(".cache/wal/colors.json"))?;
	let json: serde_json::Value = serde_json::from_str(&text).ok()?;
	let color = json
		.pointer("/colors/color4")
		.or_else(|| json.get("color4"))?;
	parse_accent_text(color.as_str()?)
}

fn config_dir(var: Env) -> Option<PathBuf> {
	var("XDG_CONFIG_HOME")
		.map(PathBuf::from)
		.filter(|dir| dir.is_absolute())
		.or_else(|| {
			let home = var("HOME").map(PathBuf::from)?;
			home.is_absolute().then(|| home.join(".config"))
		})
}

/// `SEREIN_THEME_FILE`, else `serein/theme.json` in the config directory. Debug builds follow
/// only the variable, so development sessions are not recolored by the developer's desktop.
fn theme_path(var: Env) -> Option<PathBuf> {
	if let Some(path) = var("SEREIN_THEME_FILE").filter(|path| !path.is_empty()) {
		return Some(path.into());
	}
	if cfg!(debug_assertions) {
		return None;
	}
	Some(config_dir(var)?.join("serein/theme.json"))
}

/// The value of `key` in `[section]` of an INI-style file.
fn ini_value(text: &str, section: &str, key: &str) -> Option<String> {
	let mut inside = false;
	for line in text.lines().map(str::trim) {
		if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
			inside = name == section;
		} else if inside
			&& let Some((name, value)) = line.split_once('=')
			&& name.trim() == key
		{
			return Some(value.trim().to_owned());
		}
	}
	None
}

fn read_text(path: &Path) -> Option<String> {
	use std::io::Read;
	let mut text = String::new();
	std::fs::File::open(path)
		.ok()?
		.take(MAX_FILE + 1)
		.read_to_string(&mut text)
		.ok()?;
	(text.len() as u64 <= MAX_FILE).then_some(text)
}

#[derive(Clone, Copy, Default)]
struct FileState {
	stamp: Option<SystemTime>,
	loaded: bool,
}

/// `Some` when the file's colors changed: the new colors, or `None` once the file is gone.
fn poll_file(path: &Path, state: &mut FileState) -> Option<Option<ThemeColors>> {
	let Ok(meta) = std::fs::metadata(path) else {
		state.stamp = None;
		return std::mem::take(&mut state.loaded).then_some(None);
	};
	let stamp = meta.modified().ok();
	if stamp.is_some() && stamp == state.stamp {
		return None;
	}
	// Too large to be a palette: treated like a malformed file.
	if meta.len() > MAX_FILE {
		state.stamp = stamp;
		return None;
	}
	let Some(text) = read_text(path) else {
		// Dropped, not kept, so the next poll retries: a tool rewriting the file in place can
		// fail one read.
		state.stamp = None;
		return None;
	};
	// Stored once read, so a malformed file keeps the previous colors without being re-read
	// on every poll.
	state.stamp = stamp;
	let colors = serde_json::from_str::<ThemeColors>(&text).ok()?;
	state.loaded = true;
	Some(Some(colors))
}

#[cfg(test)]
mod tests {
	use super::*;

	fn env<'a>(vars: &'a [(&'a str, &'a Path)]) -> impl Fn(&str) -> Option<OsString> + 'a {
		|name| {
			vars.iter()
				.find(|(key, _)| *key == name)
				.map(|(_, value)| value.as_os_str().to_owned())
		}
	}

	#[test]
	fn accent_text_forms() {
		assert_eq!(parse_accent_text("#3584e4"), Some([0x35, 0x84, 0xe4]));
		assert_eq!(parse_accent_text(" 3584e4\n"), Some([0x35, 0x84, 0xe4]));
		// KDE's byte triple must not be mistaken for the portal's 0..1 floats.
		assert_eq!(parse_accent_text("237,91,0"), Some([237, 91, 0]));
		assert_eq!(parse_accent_text("(0.5, 0.25, 0.0)"), Some([128, 64, 0]));
		assert_eq!(parse_accent_text("(237, 91, 0)"), Some([237, 91, 0]));
		assert_eq!(parse_accent_text("(300, 0, 0)"), None);
		assert_eq!(parse_accent_text("256,0,0"), None);
		assert_eq!(parse_accent_text("#358"), None);
		assert_eq!(parse_accent_text("nonsense"), None);
		// The portal's out-of-range channels mean "unset".
		assert_eq!(accent_from_floats(-1.0, -1.0, -1.0), None);
		assert_eq!(unpack(0), None);
		let black = [0, 0, 0];
		assert_eq!(unpack(u32::from_be_bytes([1, 0, 0, 0])), Some(black));
	}

	#[test]
	fn the_gnome_fallback_only_applies_in_a_gnome_session() {
		let gnome = Path::new("ubuntu:GNOME");
		assert!(gnome_session(&env(&[("XDG_CURRENT_DESKTOP", gnome)])));
		let kde = Path::new("KDE");
		assert!(!gnome_session(&env(&[("XDG_CURRENT_DESKTOP", kde)])));
		assert!(!gnome_session(&env(&[])));
		assert_eq!(gnome_named_accent("orange"), Some([0xed, 0x5b, 0x00]));
		assert_eq!(gnome_named_accent("magenta"), None);
	}

	#[test]
	fn kde_and_pywal_fallbacks_read_their_files() {
		let home = tempfile::tempdir().unwrap();
		let config = home.path().join("config");
		std::fs::create_dir_all(&config).unwrap();
		std::fs::create_dir_all(home.path().join(".cache/wal")).unwrap();
		let vars = [("XDG_CONFIG_HOME", config.as_path()), ("HOME", home.path())];
		let var = env(&vars);
		assert_eq!(fallback_accent(&var), None, "no files, no accent");

		std::fs::write(
			home.path().join(".cache/wal/colors.json"),
			r##"{"special": {"background": "#1a1b26"}, "colors": {"color4": "#7aa2f7"}}"##,
		)
		.unwrap();
		assert_eq!(fallback_accent(&var), Some([0x7a, 0xa2, 0xf7]));

		let kdeglobals = config.join("kdeglobals");
		std::fs::write(&kdeglobals, "[Colors:View]\nSelectionBackground=#3584e4\n").unwrap();
		assert_eq!(kde_accent(&var), Some([0x35, 0x84, 0xe4]));
		std::fs::write(
			&kdeglobals,
			"[Colors:View]\nSelectionBackground=#3584e4\n[General]\nAccentColor=237,91,0\n",
		)
		.unwrap();
		assert_eq!(
			fallback_accent(&var),
			Some([237, 91, 0]),
			"KDE outranks pywal"
		);
	}

	#[test]
	fn theme_file_polling_retries_failures_and_keeps_good_colors() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("theme.json");
		let mut state = FileState::default();
		let mut tick = 0;
		let mut rewrite = |contents: &[u8]| {
			std::fs::write(&path, contents).unwrap();
			tick += 10;
			let file = std::fs::File::options().write(true).open(&path).unwrap();
			file.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(tick))
				.unwrap();
		};
		let accent = |colors: Option<Option<ThemeColors>>| {
			colors.map(|colors| colors.and_then(|colors| colors.get("accent").cloned()))
		};

		assert_eq!(poll_file(&path, &mut state), None, "no file is quiet");
		rewrite(br##"{"accent": "#ff0000"}"##);
		assert_eq!(
			accent(poll_file(&path, &mut state)),
			Some(Some("#ff0000".into()))
		);
		assert_eq!(
			poll_file(&path, &mut state),
			None,
			"an unchanged file is not re-read"
		);

		rewrite(b"not json");
		assert_eq!(
			poll_file(&path, &mut state),
			None,
			"malformed keeps the last colors"
		);
		assert_eq!(
			poll_file(&path, &mut state),
			None,
			"and is not re-read every poll"
		);

		// A directory at the path: metadata succeeds, the read fails, and the stamp is dropped.
		std::fs::remove_file(&path).unwrap();
		std::fs::create_dir(&path).unwrap();
		assert_eq!(poll_file(&path, &mut state), None);
		assert!(
			state.stamp.is_none(),
			"a failed read must be retried on the next poll"
		);
		std::fs::remove_dir(&path).unwrap();

		rewrite(br##"{"accent": "#00ff00"}"##);
		assert_eq!(
			accent(poll_file(&path, &mut state)),
			Some(Some("#00ff00".into()))
		);
		rewrite(&vec![b' '; MAX_FILE as usize + 1]);
		assert_eq!(
			poll_file(&path, &mut state),
			None,
			"an oversized file is ignored"
		);

		std::fs::remove_file(&path).unwrap();
		assert_eq!(
			accent(poll_file(&path, &mut state)),
			Some(None),
			"removal clears"
		);
		assert_eq!(
			poll_file(&path, &mut state),
			None,
			"staying gone stays quiet"
		);
	}
}
