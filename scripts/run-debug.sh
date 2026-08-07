#!/usr/bin/env bash
set -euo pipefail

project_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$project_dir"

appimage="${LIVESCREEN_APPIMAGE:-$project_dir/LiveScreen-x86_64.AppImage}"
if [[ ! -x "$appimage" ]]; then
    appimage="$project_dir/target/debug/livescreen"
fi
if [[ ! -x "$appimage" ]]; then
    echo "Не найден исполняемый LiveScreen: $appimage" >&2
    echo "Сначала соберите AppImage или cargo build." >&2
    exit 1
fi

if [[ -n "${LIVESCREEN_DEBUG_DIR:-}" ]]; then
    log_dir="$LIVESCREEN_DEBUG_DIR"
    mkdir -p "$log_dir"
else
    log_dir="$(mktemp -d /tmp/livescreen-debug.XXXXXX)"
fi
mkdir -p "$log_dir/app" "$log_dir/dot"

export LIVESCREEN_LOG=debug
export LIVESCREEN_LOG_DIR="$log_dir/app"
export GST_DEBUG_NO_COLOR=1
export GST_DEBUG="${LIVESCREEN_GST_DEBUG:-*:2,pipewiresrc:4,GST_STATES:3}"
export GST_DEBUG_FILE="$log_dir/gstreamer.log"
export GST_DEBUG_DUMP_DOT_DIR="$log_dir/dot"
export PIPEWIRE_DEBUG="${LIVESCREEN_PIPEWIRE_DEBUG:-0}"
export PIPEWIRE_LOG="$log_dir/pipewire.log"

cat > "$log_dir/environment.txt" <<EOF
application=$appimage
LIVESCREEN_LOG=$LIVESCREEN_LOG
LIVESCREEN_LOG_DIR=$LIVESCREEN_LOG_DIR
GST_DEBUG=$GST_DEBUG
GST_DEBUG_FILE=$GST_DEBUG_FILE
GST_DEBUG_DUMP_DOT_DIR=$GST_DEBUG_DUMP_DOT_DIR
PIPEWIRE_DEBUG=$PIPEWIRE_DEBUG
PIPEWIRE_LOG=$PIPEWIRE_LOG
started_at=$(date --iso-8601=seconds)
EOF

printf 'Диагностический запуск LiveScreen\n'
printf 'Приложение: %s\n' "$appimage"
printf 'Артефакты:  %s\n' "$log_dir"
printf 'После воспроизведения передайте содержимое этого каталога.\n'

set +e
"$appimage" "$@" \
    > >(tee "$log_dir/stdout.log") \
    2> >(tee "$log_dir/stderr.log" >&2)
status=$?
set -e

printf 'LiveScreen завершился с кодом %s\n' "$status"
printf 'Логи: %s\n' "$log_dir"
exit "$status"
