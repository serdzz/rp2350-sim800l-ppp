#!/usr/bin/env bash
# Прогон хостовых тестов AT-слоя.
#
# Нужен явный --target: в .cargo/config.toml прописан thumbv8m, а эти тесты
# должны исполняться на машине разработчика.
set -euo pipefail
cd "$(dirname "$0")/at-tests"
exec cargo test --target "$(rustc -vV | sed -n 's/^host: //p')" "$@"
