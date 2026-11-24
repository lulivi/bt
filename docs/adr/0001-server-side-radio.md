# The server is an internet radio

Listeners must hear the same music at the same time, including on phones with the screen locked. An embedded YouTube player in each browser pauses when a phone locks and needs every client kept in sync, so instead the server downloads each song's audio with `yt-dlp`, plays the queue itself and broadcasts one continuous MP3 stream at `GET /stream` that every browser plays with `<audio>`. Sync comes for free (only the 1–4 s of client buffering differs), and pausing means the server sends silence, so the stream never stops and phones keep the audio session alive.

## Consequences

- Extracting audio with `yt-dlp` goes against YouTube's Terms of Service: this is for private use, at our own risk, and `yt-dlp` must be kept up to date.
- The server needs `ffmpeg`, `yt-dlp` and a JS runtime (`deno`) for `yt-dlp`.
- Per-listener features (seek, local pause) don't fit: everyone hears the same broadcast.
