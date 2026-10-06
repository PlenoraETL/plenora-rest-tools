#!/usr/bin/env bash
# Esegue una fase della campagna operativa (smoke, load, soak) in locale, su
# una VM Linux o nel workflow Campaign, e scrive report JSON e Markdown.
#
#   scripts/campaign.sh FASE [--quick] [--duration-min N] [--seed N]
#                            [--out-dir DIR] [--work-dir DIR]
#
# Il binario si compila in release con la toolchain dei gate
# (CAMPAIGN_RUST_TOOLCHAIN, default 1.98.1) e Cargo.lock. Il report si chiama
# <out-dir>/<data UTC>-<fase>[-quick].json|md e registra commit, toolchain e
# host. Un albero di lavoro con modifiche non committate viene dichiarato nel
# commit del report (suffisso "+modifiche-locali"), mai nascosto.
#
# Exit code: quello del binario (0 superata, 1 criteri falliti, 2 uso o
# configurazione, 3 errore dell'harness).
set -euo pipefail

usage() {
    sed -n '2,15p' "$0" | sed 's/^# \{0,1\}//'
    exit 2
}

[ $# -ge 1 ] || usage
phase="$1"
shift
case "$phase" in
    smoke | load | soak) ;;
    *) usage ;;
esac

root="$(cd "$(dirname "$0")/.." && pwd)"
out_dir="$root/campaign-out"
work_dir=""
quick=""
passthrough=()
while [ $# -gt 0 ]; do
    case "$1" in
        --quick)
            quick="-quick"
            passthrough+=("--quick")
            shift
            ;;
        --duration-min | --seed)
            [ $# -ge 2 ] || usage
            passthrough+=("$1" "$2")
            shift 2
            ;;
        --out-dir)
            [ $# -ge 2 ] || usage
            out_dir="$2"
            shift 2
            ;;
        --work-dir)
            [ $# -ge 2 ] || usage
            work_dir="$2"
            shift 2
            ;;
        *) usage ;;
    esac
done

toolchain="${CAMPAIGN_RUST_TOOLCHAIN:-1.98.1}"
target_dir="${CARGO_TARGET_DIR:-$root/target}"

cd "$root"
commit="$(git rev-parse HEAD)"
if [ -n "$(git status --porcelain --untracked-files=no)" ]; then
    commit="$commit+modifiche-locali"
fi

cargo "+$toolchain" build --release --locked -p plenora-rest-campaign
binary="$target_dir/release/plenora-rest-campaign"

export CAMPAIGN_COMMIT="$commit"
export CAMPAIGN_TOOLCHAIN
CAMPAIGN_TOOLCHAIN="$(rustc "+$toolchain" -V)"
export CAMPAIGN_HOST="${CAMPAIGN_HOST:-$(uname -srm)}"

mkdir -p "$out_dir"
prefix="$out_dir/$(date -u +%Y-%m-%d)-$phase$quick"
arguments=(--phase "$phase" --out "$prefix" ${passthrough[@]+"${passthrough[@]}"})
if [ -n "$work_dir" ]; then
    arguments+=(--work-dir "$work_dir")
fi

echo "campagna: fase $phase$quick, commit $commit, report $prefix.json" >&2
status=0
"$binary" "${arguments[@]}" || status=$?
exit "$status"
