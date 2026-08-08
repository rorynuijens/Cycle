//! Where Cycle keeps its data on disk.
//!
//! One function decides this, because three copies of the answer is how a rider
//! ends up with two training histories: the flatpak build resolves
//! `XDG_DATA_HOME` inside its sandbox, a `cargo run` build resolves it on the
//! host, and both then look like the real thing. Debug builds are therefore
//! given a directory of their own, so development can never write into the
//! history the installed app owns.

use std::path::PathBuf;

/// Replaces the data directory wholesale. Set this to point a development build
/// at real data deliberately — e.g. the flatpak's own data directory.
pub const DATA_HOME_ENV: &str = "CYCLE_DATA_HOME";

/// Directory holding the database, saved routes and exported activities.
///
/// Release builds use `<XDG_DATA_HOME>/cycle`; debug builds use
/// `<XDG_DATA_HOME>/cycle-dev`. [`DATA_HOME_ENV`] overrides both.
pub fn data_dir() -> PathBuf {
    resolve(
        std::env::var_os(DATA_HOME_ENV).map(PathBuf::from),
        std::env::var_os("XDG_DATA_HOME").map(PathBuf::from),
        std::env::var_os("HOME").map(PathBuf::from),
        cfg!(debug_assertions),
    )
}

/// The directory name a build of this kind owns.
fn dir_name(debug: bool) -> &'static str {
    if debug {
        "cycle-dev"
    } else {
        "cycle"
    }
}

/// Pure form of [`data_dir`], so the precedence rules can be tested without
/// mutating the environment of a running test binary.
fn resolve(
    override_dir: Option<PathBuf>,
    xdg_data_home: Option<PathBuf>,
    home: Option<PathBuf>,
    debug: bool,
) -> PathBuf {
    // An override names the directory itself, not a parent to append to: that is
    // what makes it able to reach an existing directory of either kind.
    if let Some(dir) = override_dir.filter(|p| !p.as_os_str().is_empty()) {
        return dir;
    }

    // The XDG spec treats an empty variable exactly like an unset one; taking it
    // literally would put the database at "/cycle/cycle.db".
    let base = xdg_data_home
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| {
            home.filter(|p| !p.as_os_str().is_empty())
                .unwrap_or_else(|| PathBuf::from("/tmp"))
                .join(".local/share")
        });

    base.join(dir_name(debug))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_use_the_release_directory_for_release_builds() {
        let dir = resolve(None, Some("/data".into()), None, false);
        assert_eq!(dir, PathBuf::from("/data/cycle"));
    }

    #[test]
    fn should_use_a_separate_directory_for_debug_builds() {
        let dir = resolve(None, Some("/data".into()), None, true);
        assert_eq!(dir, PathBuf::from("/data/cycle-dev"));
    }

    #[test]
    fn should_never_share_a_directory_between_build_kinds() {
        let release = resolve(None, Some("/data".into()), None, false);
        let debug = resolve(None, Some("/data".into()), None, true);
        assert_ne!(
            release, debug,
            "a development build must not write into the installed app's history"
        );
    }

    #[test]
    fn should_let_an_override_reach_the_release_directory_from_a_debug_build() {
        let dir = resolve(
            Some("/data/cycle".into()),
            Some("/other".into()),
            None,
            true,
        );
        assert_eq!(dir, PathBuf::from("/data/cycle"));
    }

    #[test]
    fn should_fall_back_to_home_when_xdg_data_home_is_unset() {
        let dir = resolve(None, None, Some("/home/rider".into()), false);
        assert_eq!(dir, PathBuf::from("/home/rider/.local/share/cycle"));
    }

    #[test]
    fn should_treat_an_empty_xdg_data_home_as_unset() {
        // Taking "" literally would resolve the database to "/cycle/cycle.db".
        let dir = resolve(None, Some("".into()), Some("/home/rider".into()), false);
        assert_eq!(dir, PathBuf::from("/home/rider/.local/share/cycle"));
    }

    #[test]
    fn should_treat_an_empty_override_as_unset() {
        let dir = resolve(Some("".into()), Some("/data".into()), None, false);
        assert_eq!(dir, PathBuf::from("/data/cycle"));
    }

    #[test]
    fn should_fall_back_to_tmp_when_neither_xdg_nor_home_is_set() {
        let dir = resolve(None, None, None, false);
        assert_eq!(dir, PathBuf::from("/tmp/.local/share/cycle"));
    }
}
