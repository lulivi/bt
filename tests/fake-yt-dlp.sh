#!/bin/sh
# SPDX-FileCopyrightText: 2026 Lorena Jiménez, Luis Liñán
# SPDX-License-Identifier: MIT
#
# Stands in for yt-dlp in tests (CI can't rely on YouTube).
#   yt-dlp -J --no-playlist -- <id>                 → metadata JSON
#   yt-dlp -f bestaudio -o <tmpl> --print … -- <id> → writes a sine wave, prints its path
# The ID "unavailable" fails like a removed video.
# FAKE_YTDLP_LOG: every call is appended here. FAKE_YTDLP_SECONDS: audio length (default 2).
set -eu
prev_arg=

if [ -n "${FAKE_YTDLP_LOG:-}" ]; then
  echo "$*" >>"$FAKE_YTDLP_LOG"
fi

mode=download
out=
for arg in "$@"; do
  case "$prev_arg" in -o) out=$arg ;; esac
  case "$arg" in -J) mode=metadata ;; esac
  prev_arg=$arg
  id=$arg
done

if [ "$id" = unavailable ]; then
  echo "ERROR: [youtube] $id: Video unavailable" >&2
  exit 1
fi

seconds=${FAKE_YTDLP_SECONDS:-2}
if [ "$mode" = metadata ]; then
  printf '{"id": "%s", "title": "Song %s", "duration": %s}\n' "$id" "$id" "$seconds"
  exit 0
fi

path=$(printf '%s' "$out" | sed 's/%(ext)s/wav/')
"${BT_FFMPEG:-ffmpeg}" -nostdin -hide_banner -loglevel error -y \
  -f lavfi -i "sine=frequency=440:duration=$seconds" "$path"
echo "$path"
