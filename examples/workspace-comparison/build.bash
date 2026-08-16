#!/usr/bin/env bash
set -euo pipefail

cd -- "$(dirname -- "${BASH_SOURCE[0]}")"
out=${1:-.work/bash}
rm -rf -- "$out"
mkdir -p -- "$out/published" "$out/drafts"

build_document() {
  local name=$1 state=$2 source=$3 destination digest

  [[ -f $source ]] || { printf 'missing document: %s\n' "$source" >&2; return 1; }
  case $state in
    published) destination="published/$name.md" ;;
    draft) destination="drafts/$name.md" ;;
    *) printf 'invalid state for %s: %s\n' "$name" "$state" >&2; return 1 ;;
  esac

  cp -- "$source" "$out/$destination"
  digest=$(sha256sum -- "$source")
  digest=${digest%% *}
  printf '%s\t%s\t%s\t%s\n' "$name" "$state" "$destination" "$digest" >> "$out/index.tsv"
}

while IFS=$'\t' read -r name state source; do
  if [[ -n $name ]]; then
    build_document "$name" "$state" "$source"
  fi
done < workspace.tsv
