#!/usr/bin/env bash
set -euo pipefail

seed_root=/run/auth-seed
runtime_home=${HOME:?HOME must be set}
umask 077

copy_named_files() {
  local relative=$1
  shift
  local source_path="$seed_root/$relative"
  local target_path="$runtime_home/$relative"
  [[ -d "$source_path" ]] || return 0
  mkdir -p "$target_path"
  local name
  for name in "$@"; do
    if [[ -f "$source_path/$name" ]]; then
      cp --no-preserve=mode,ownership -- "$source_path/$name" "$target_path/$name"
    fi
  done
  chmod -R u+rwX,go-rwx "$target_path"
}

copy_named_files .claude .credentials.json settings.json settings.local.json
copy_named_files .codex auth.json config.toml hooks.json
copy_named_files .local/share/opencode auth.json account.json
copy_named_files .local/share/kilo auth.json account.json

exec /usr/local/bin/lumen-api
