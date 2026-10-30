#!/bin/sh
# CoreLocationAgent는 번들이 없는 실행 파일의 위치 권한 요청을 "client bundle is NULL"로 무시한다.
# cargo run으로 만든 바이너리를 .app 번들로 감싸 실행한다. 테스트 바이너리는 그대로 실행한다.
set -eu

bin="$1"
shift
case "$bin" in
*/deps/*) exec "$bin" "$@" ;;
esac

root="$(cd "$(dirname "$0")/.." && pwd)"
app="$(dirname "$bin")/SharedMap.app"
mkdir -p "$app/Contents/MacOS"
cp "$root/crates/app/macos/Info.plist" "$app/Contents/Info.plist"
# 실행 중인 이전 바이너리를 덮어쓰면 코드 서명이 깨져 종료되므로 새 파일로 교체한다.
rm -f "$app/Contents/MacOS/shared-map"
cp "$bin" "$app/Contents/MacOS/shared-map"
codesign --force --sign - "$app" >/dev/null 2>&1 || echo "macos-run: 번들 ad-hoc 서명 실패" >&2
exec "$app/Contents/MacOS/shared-map" "$@"
