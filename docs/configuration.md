# Configuration

bt reads its settings from a [TOML](https://toml.io) file, `~/.config/bt/config.toml` (or `$XDG_CONFIG_HOME/bt/config.toml`), or the file in `BT_CONFIG`. Every setting is optional, and so is the file: without it bt uses the defaults. A `BT_CONFIG` that names a missing file, an unknown key or an invalid value stops bt on startup, naming the file and key.

Each setting also has a `BT_*` environment variable, which overrides it.

| Key | Variable | Default | Meaning |
|---|---|---|---|
| `addr` | `BT_ADDR` | `127.0.0.1:3000` | Address the app listens on |
| `cache_dir` | `BT_CACHE_DIR` | `~/.local/share/bt/cache` | Downloaded audio; safe to delete, songs download again |
| `state_file` | `BT_STATE_FILE` | `~/.local/share/bt/state.json` | The queue, saved after every change and restored (paused) on startup |
| `ffmpeg` | `BT_FFMPEG` | `ffmpeg` (from `PATH`) | ffmpeg binary |
| `ytdlp` | `BT_YTDLP` | `yt-dlp` (from `PATH`) | yt-dlp binary |
| `public_url` | `BT_PUBLIC_URL` | — | The origin listeners open, e.g. `https://bt.example.com` (no path: bt runs at the root of its host); required with access links |
| `[access]` | `BT_ACCESS_TOKENS` | none (anyone can use bt) | [Access links](access.md): a `name = "uuid"` table, or `name:uuid,name:uuid` in the variable, which replaces the whole table |

`~/.local/share` is `$XDG_DATA_HOME` if that's set. Relative paths are relative to the directory bt runs in; `~` is not expanded, so write absolute paths.

[`deploy/config.example.toml`](../deploy/config.example.toml) is a template with every key. A private bt behind a reverse proxy needs just:

```toml
addr = "127.0.0.1:3000"
public_url = "https://bt.example.com"

[access]
lorena = "<uuid>"
luis = "<uuid>"
```

bt logs which config file it read on startup.
