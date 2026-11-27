// SPDX-FileCopyrightText: 2026 Lorena Jiménez, Luis Liñán
// SPDX-License-Identifier: MIT

//! The Operator's settings: a TOML config file, overridden by the `BT_*` variables.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::Context;
use serde::Deserialize;

use crate::auth::{self, Access};

/// The config file's contents; every key is optional.
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct File {
    /// See [`Settings::addr`].
    addr: Option<String>,
    /// See [`Settings::cache_dir`].
    cache_dir: Option<String>,
    /// See [`Settings::state_file`].
    state_file: Option<String>,
    /// See [`Settings::ffmpeg`].
    ffmpeg: Option<String>,
    /// See [`Settings::ytdlp`].
    ytdlp: Option<String>,
    /// The origin Listeners open; required with `access`.
    public_url: Option<String>,
    /// Each invited person's name and token.
    access: Option<BTreeMap<String, String>>,
}

/// Everything bt is configured with.
pub struct Settings {
    /// The config file that was read, if any.
    pub file: Option<PathBuf>,
    /// The address to listen on.
    pub addr: SocketAddr,
    /// Where audio is downloaded.
    pub cache_dir: PathBuf,
    /// Where the Queue is saved.
    pub state_file: PathBuf,
    /// The ffmpeg binary.
    pub ffmpeg: String,
    /// The yt-dlp binary.
    pub ytdlp: String,
    /// The Access links, or `None` to let anyone in.
    pub access: Option<Access>,
}

/// Loads the settings, reading variables with `env`.
///
/// The config file is the one in `BT_CONFIG`, else `bt/config.toml` in the XDG config directory
/// (`$XDG_CONFIG_HOME`, else `~/.config`); a missing file there means all defaults. The cache and
/// state file default to `bt` in the XDG data directory (`$XDG_DATA_HOME`, else
/// `~/.local/share`), or to the working directory without either. Each `BT_*`
/// variable overrides its key in the file: `BT_ACCESS_TOKENS` replaces the whole `access` table.
///
/// # Errors
///
/// Fails if the file in `BT_CONFIG` is missing, the file can't be read or has an unknown key, or a
/// value is invalid. Errors name the file and key, or the variable, the value came from.
pub fn load(env: impl Fn(&str) -> Option<String>) -> anyhow::Result<Settings> {
    let (path, required) = match env("BT_CONFIG") {
        Some(path) => (Some(PathBuf::from(path)), true),
        None => (
            xdg(&env, "XDG_CONFIG_HOME", ".config").map(|d| d.join("bt/config.toml")),
            false,
        ),
    };
    let text = match &path {
        Some(path) => match std::fs::read_to_string(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && !required => None,
            text => Some(text.with_context(|| format!("reading {}", path.display()))?),
        },
        None => None,
    };
    let file: File = match &text {
        Some(text) => toml::from_str(text).with_context(|| path_of(&path))?,
        None => File::default(),
    };
    let shown = path_of(&path);
    // A value from `var`, else from the file's `key`, with where it came from.
    let pick = |var: &str, key: &str, value: Option<String>| {
        env(var)
            .map(|v| (v, var.to_string()))
            .or_else(|| value.map(|v| (v, format!("{shown}: {key}"))))
    };

    let addr = match pick("BT_ADDR", "addr", file.addr) {
        Some((addr, from)) => addr
            .parse()
            .with_context(|| format!("{from} must be <ip>:<port>"))?,
        None => SocketAddr::from(([127, 0, 0, 1], 3000)),
    };
    let or = |picked: Option<(String, String)>, default: &str| {
        picked.map_or_else(|| default.to_string(), |(v, _)| v)
    };
    // bt's files go in `bt` in the XDG data directory, else in the working directory.
    let data =
        xdg(&env, "XDG_DATA_HOME", ".local/share").map_or_else(PathBuf::new, |d| d.join("bt"));
    let cache_dir = pick("BT_CACHE_DIR", "cache_dir", file.cache_dir)
        .map_or_else(|| data.join("cache"), |(v, _)| v.into());
    let state_file = pick("BT_STATE_FILE", "state_file", file.state_file)
        .map_or_else(|| data.join("state.json"), |(v, _)| v.into());
    let ffmpeg = or(pick("BT_FFMPEG", "ffmpeg", file.ffmpeg), "ffmpeg");
    let ytdlp = or(pick("BT_YTDLP", "ytdlp", file.ytdlp), "yt-dlp");

    let public_url = pick("BT_PUBLIC_URL", "public_url", file.public_url)
        .map(|(url, from)| auth::public_url(&url).context(from))
        .transpose()?;
    let tokens = match (env("BT_ACCESS_TOKENS"), file.access) {
        (Some(tokens), _) => Some((
            auth::parse(&tokens).context("BT_ACCESS_TOKENS")?,
            "BT_ACCESS_TOKENS".into(),
        )),
        (None, Some(table)) => {
            let from = format!("{shown}: access");
            let tokens = table
                .into_iter()
                .map(|(name, t)| auth::token(&name, &t).map(|t| (name, t)))
                .collect::<anyhow::Result<Vec<_>>>()
                .with_context(|| from.clone())?;
            Some((tokens, from))
        }
        (None, None) => None,
    };
    let access = match tokens {
        Some((tokens, from)) => {
            auth::validate(&tokens).with_context(|| from.clone())?;
            let url =
                public_url.with_context(|| format!("{from} needs public_url or BT_PUBLIC_URL"))?;
            Some(Access::new(tokens, &url))
        }
        None => None,
    };

    Ok(Settings {
        file: text.and(path),
        addr,
        cache_dir,
        state_file,
        ffmpeg,
        ytdlp,
        access,
    })
}

/// Returns `path`'s display form, or an empty string if there is none.
fn path_of(path: &Option<PathBuf>) -> String {
    path.as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_default()
}

/// Returns the XDG base directory in `var`, else `fallback` under `$HOME`.
///
/// A relative `var` is ignored, as the XDG spec says. Returns `None` without either.
fn xdg(env: impl Fn(&str) -> Option<String>, var: &str, fallback: &str) -> Option<PathBuf> {
    let absolute = |v: String| Some(PathBuf::from(v)).filter(|p| p.is_absolute());
    env(var)
        .and_then(absolute)
        .or_else(|| env("HOME").and_then(absolute).map(|h| h.join(fallback)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A valid token.
    const A: &str = "67e55044-10b1-426f-9247-bb680e5fe0c8";
    /// Another valid token.
    const B: &str = "a1a2a3a4-b1b2-c1c2-d1d2-d3d4d5d6d7d8";

    /// Loads the settings with only the variables `vars` set, and `config` as the config file in
    /// `$XDG_CONFIG_HOME` (none if it's `None`).
    fn load_with(config: Option<&str>, vars: &[(&str, &str)]) -> anyhow::Result<Settings> {
        let dir = tempfile::tempdir().unwrap();
        if let Some(config) = config {
            std::fs::create_dir(dir.path().join("bt")).unwrap();
            std::fs::write(dir.path().join("bt/config.toml"), config).unwrap();
        }
        let home = dir.path().to_str().unwrap().to_string();
        load(|name| {
            if name == "XDG_CONFIG_HOME" {
                return Some(home.clone());
            }
            vars.iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v.to_string())
        })
    }

    /// Returns the error's whole chain on one line.
    fn error(result: anyhow::Result<Settings>) -> String {
        format!("{:#}", result.err().expect("should fail"))
    }

    #[test]
    fn no_file_means_defaults() {
        let s = load_with(None, &[]).unwrap();
        assert!(s.file.is_none());
        assert_eq!(s.addr.to_string(), "127.0.0.1:3000");
        assert_eq!(s.cache_dir, PathBuf::from("cache"));
        assert_eq!(s.state_file, PathBuf::from("state.json"));
        assert_eq!((s.ffmpeg.as_str(), s.ytdlp.as_str()), ("ffmpeg", "yt-dlp"));
        assert!(s.access.is_none());

        let s = load_with(None, &[("HOME", "/h")]).unwrap();
        assert_eq!(s.cache_dir, PathBuf::from("/h/.local/share/bt/cache"));
        let s = load_with(None, &[("HOME", "/h"), ("XDG_DATA_HOME", "/d")]).unwrap();
        assert_eq!(s.state_file, PathBuf::from("/d/bt/state.json"));
    }

    #[test]
    fn a_missing_bt_config_fails() {
        let e = error(load_with(None, &[("BT_CONFIG", "/nonexistent/bt.toml")]));
        assert!(e.starts_with("reading /nonexistent/bt.toml"), "{e}");
    }

    #[test]
    fn reads_the_file_and_env_overrides_it() {
        let config = format!(
            "addr = \"0.0.0.0:80\"\ncache_dir = \"/c\"\nstate_file = \"/s.json\"\n\
             ffmpeg = \"/f\"\nytdlp = \"/y\"\npublic_url = \"https://bt.example.com\"\n\
             [access]\nlorena = \"{A}\"\n"
        );
        let s = load_with(Some(&config), &[]).unwrap();
        assert!(s.file.unwrap().ends_with("bt/config.toml"));
        assert_eq!(s.addr.to_string(), "0.0.0.0:80");
        assert_eq!(s.cache_dir, PathBuf::from("/c"));
        assert_eq!(s.state_file, PathBuf::from("/s.json"));
        assert_eq!((s.ffmpeg.as_str(), s.ytdlp.as_str()), ("/f", "/y"));
        let links: Vec<_> = s
            .access
            .unwrap()
            .links()
            .map(|(n, _)| n.to_string())
            .collect();
        assert_eq!(links, ["lorena"]);

        let tokens = format!("luis:{B}");
        let s = load_with(
            Some(&config),
            &[("BT_ADDR", "127.0.0.1:1"), ("BT_ACCESS_TOKENS", &tokens)],
        )
        .unwrap();
        assert_eq!(s.addr.to_string(), "127.0.0.1:1");
        assert_eq!(s.cache_dir, PathBuf::from("/c"));
        let links: Vec<_> = s
            .access
            .unwrap()
            .links()
            .map(|(n, _)| n.to_string())
            .collect();
        assert_eq!(links, ["luis"]);
    }

    #[test]
    fn errors_name_the_file_and_key_or_the_variable() {
        for (config, vars, want) in [
            ("port = 1", &[][..], "config.toml: TOML parse error"),
            ("port = 1", &[], "unknown field `port`"),
            ("addr = 1", &[], "invalid type: integer"),
            ("addr = \"x\"", &[], "config.toml: addr must be <ip>:<port>"),
            ("", &[("BT_ADDR", "x")], "BT_ADDR must be <ip>:<port>"),
            ("[access]", &[], "config.toml: access: no entries"),
            (
                "[access]\nlorena = \"x\"",
                &[],
                "access: lorena's token is not a UUID",
            ),
            (
                "",
                &[("BT_ACCESS_TOKENS", "")],
                "BT_ACCESS_TOKENS: no entries",
            ),
            (
                &format!("[access]\nlorena = \"{A}\""),
                &[],
                "config.toml: access needs public_url or BT_PUBLIC_URL",
            ),
            (
                &format!("public_url = \"ftp://x\"\n[access]\nlorena = \"{A}\""),
                &[],
                "config.toml: public_url: must be http or https",
            ),
            (
                "public_url = \"https://x/bt\"",
                &[],
                "config.toml: public_url: must not have a path",
            ),
            (
                "",
                &[("BT_PUBLIC_URL", "x")],
                "BT_PUBLIC_URL: must be a URL",
            ),
        ] {
            let e = error(load_with(Some(config), vars));
            assert!(e.contains(want), "{config:?}: {e}");
            assert!(!e.contains(A), "{e}");
        }
    }

    #[test]
    fn xdg_ignores_relative_dirs() {
        let env = |vars: &'static [(&str, &str)]| {
            move |name: &str| {
                vars.iter()
                    .find(|(k, _)| *k == name)
                    .map(|(_, v)| v.to_string())
            }
        };
        let dir = |vars| xdg(env(vars), "XDG_CONFIG_HOME", ".config");
        assert_eq!(dir(&[("XDG_CONFIG_HOME", "/x")]), Some("/x".into()));
        assert_eq!(
            dir(&[("XDG_CONFIG_HOME", "x"), ("HOME", "/h")]),
            Some("/h/.config".into())
        );
        assert_eq!(dir(&[]), None);
    }
}
