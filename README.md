<p align="center">
  <img src="logo.png" alt="bt — Better Together" width="200">
</p>

# In sync. 🎧

Some songs sound better together.

This project started with a simple idea: distance shouldn’t stop us from listening to music together. An app that lets two people share the same music, in real time, wherever they are.

Discover new songs, send a track that says it all, or simply keep each other company without having to fill the silence.

---

### 🎶 The idea

A shared space where listening to music becomes something you do together. You bring your person; the music does the rest.

### 🚧 Work in progress

We’re building the experience, one song at a time. The goal: less friction, more shared moments.

### 📑 Contents

- [How it works](#-how-it-works)
- [Requirements](#requirements)
- [Develop](#develop)
- [Download a release](#download-a-release)
- [Docs](#docs): configuration, access links, deploy, releasing
- [License](#-license)

---

## 📻 How it works

bt is an internet radio: the server downloads each song's audio with `yt-dlp`, plays the shared queue itself and broadcasts one MP3 stream that every listener's browser plays ([why](docs/adr/0001-server-side-radio.md)).

> Extracting audio with yt-dlp goes against YouTube's Terms of Service. This is for private use, at your own risk. Keep yt-dlp up to date (`yt-dlp -U`).

## Requirements

- [rustup](https://rustup.rs) (the toolchain in `rust-toolchain.toml` installs itself)
- `ffmpeg`
- [`yt-dlp`](https://github.com/yt-dlp/yt-dlp#installation) (the standalone release build) and [`deno`](https://deno.com) (yt-dlp needs a JS runtime for YouTube)
- For `make check`: `shellcheck` and [`actionlint`](https://github.com/rhysd/actionlint)

## Develop

```sh
make run    # then open http://localhost:3000 and press "Connect audio"
make check  # fmt, clippy, tests, shellcheck, actionlint (what CI runs)
```

The tests use real ffmpeg and a fake yt-dlp (`tests/fake-yt-dlp.sh`), so they don't need network access.

## Download a release

The [latest release](https://github.com/lulivi/bt/releases/latest) has static Linux binaries for `x86_64` and `aarch64`. They need `ffmpeg`, `yt-dlp` and `deno` in `PATH` (see [Requirements](#requirements)).

```sh
target=x86_64-unknown-linux-musl   # or aarch64-unknown-linux-musl
url=https://github.com/lulivi/bt/releases/latest/download
curl -fLO "$url/bt-$target.tar.gz" -fLO "$url/bt-$target.tar.gz.sha256"
sha256sum -c "bt-$target.tar.gz.sha256"
tar -xzf "bt-$target.tar.gz" bt
./bt   # then open http://localhost:3000 and press "Connect audio"
```

## Docs

- [Configuration](docs/configuration.md): the `BT_*` variables
- [Access links](docs/access.md): keep bt private to the people you invite
- [Deploy](docs/deploy.md): install bt on a host as a systemd service
- [Releasing](docs/releasing.md)

---

## 📄 License

[MIT](LICENSE) © 2026 Lorena Jiménez, Luis Liñán
