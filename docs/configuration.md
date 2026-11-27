# Configuration

All environment variables are optional.

| Var | Default | Meaning |
|---|---|---|
| `BT_ADDR` | `127.0.0.1:3000` | Address the app listens on |
| `BT_CACHE_DIR` | `./cache` | Downloaded audio |
| `BT_STATE_FILE` | `./state.json` | The queue, saved after every change and restored (paused) on startup |
| `BT_FFMPEG` | `ffmpeg` (from `PATH`) | ffmpeg binary |
| `BT_YTDLP` | `yt-dlp` (from `PATH`) | yt-dlp binary |
| `BT_ACCESS_TOKENS` | unset (anyone can use bt) | [Access links](access.md), as `name:uuid,name:uuid` |
| `BT_PUBLIC_URL` | — | The URL listeners open, e.g. `https://bt.example.com`; required with `BT_ACCESS_TOKENS` |
