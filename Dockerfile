# SPDX-FileCopyrightText: 2026 Lorena Jiménez, Luis Liñán
# SPDX-License-Identifier: MIT

# bt with ffmpeg, Deno and yt-dlp, from a static bt built on the host: run `make docker-run`.

ARG DENO_VERSION=2.9.8
FROM denoland/deno:bin-${DENO_VERSION} AS deno

FROM debian:trixie-slim
ARG TARGETARCH
ARG TARGET
RUN apt-get update \
 && apt-get install -y --no-install-recommends ffmpeg ca-certificates curl \
 && rm -rf /var/lib/apt/lists/*
COPY --from=deno /deno /usr/local/bin/deno
RUN f=yt-dlp_linux; [ "$TARGETARCH" = arm64 ] && f=yt-dlp_linux_aarch64; \
    mkdir /opt/yt-dlp \
 && curl -fsSL -o /opt/yt-dlp/yt-dlp "https://github.com/yt-dlp/yt-dlp/releases/latest/download/$f" \
 && chmod 777 /opt/yt-dlp /opt/yt-dlp/yt-dlp
COPY target/${TARGET}/release/bt /usr/local/bin/bt
ENV PATH=/opt/yt-dlp:$PATH \
    BT_ADDR=0.0.0.0:3000 \
    BT_CACHE_DIR=/data/cache \
    BT_STATE_FILE=/data/state.json \
    XDG_CACHE_HOME=/tmp/cache
VOLUME /data
EXPOSE 3000
ENTRYPOINT ["sh", "-c", "yt-dlp -U || true; exec bt"]
