# Deploy

`make deploy` cross-compiles a static binary and installs it on any Linux host over SSH as a systemd **user** service (`deploy/bt.service`).

On the host, once:
- put `ffmpeg`, `yt-dlp` and `deno` in `PATH` (`~/.local/bin` works);
- run `loginctl enable-linger <user>` so the service runs without an open session.

Then, from your machine:

```sh
make deploy DEPLOY_HOST=<ssh host>   # DEPLOY_TARGET=x86_64-unknown-linux-musl for x86_64 hosts
make logs DEPLOY_HOST=<ssh host>
```

Audio is cached in `~/.local/share/bt/cache`, and the queue is saved in `~/.local/share/bt/state.json`. To configure bt, copy [`deploy/config.example.toml`](../deploy/config.example.toml) to `~/.config/bt/config.toml` on the host, edit it (see [Configuration](configuration.md)) and run `systemctl --user restart bt`; `make deploy` never touches that file. If listeners connect from outside your network, put a reverse proxy with HTTPS in front of `addr`.
