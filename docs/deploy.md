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

The service runs from `~/.local/share/bt` (so audio is cached in `~/.local/share/bt/cache`, and the queue is saved in `~/.local/share/bt/state.json`). Set the [`BT_*` variables](configuration.md) with `systemctl --user edit bt`. If listeners connect from outside your network, put a reverse proxy with HTTPS in front of `BT_ADDR`.
