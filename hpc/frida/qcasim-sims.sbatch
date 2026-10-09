#!/usr/bin/env bash
# Simulates a list of design files with `qca-sim sim` (and optionally prints
# their truth tables with `qca-sim truth`), e.g. the variants written by
# scripts/gen_designs.py. Each array task takes every N-th file of the list and
# runs up to $SLURM_CPUS_PER_TASK simulations at the same time. A design whose
# .qcs file already exists is skipped, so a failed or timed-out array can
# simply be submitted again.
#
#   ls $PWD/designs/*.qcd > designs.txt
#   sbatch -p amd --array=0-7 -c 32 qcasim-sims.sbatch designs.txt [truth options]
#
# Truth tables (with the given `qca-sim truth` options, e.g. -d Q:1) are
# written next to each .qcs file as <name>.truth.txt when the variable
# QCASIM_TRUTH=1 is exported.
#SBATCH --job-name=qcasim-sims
#SBATCH --cpus-per-task=16
#SBATCH --mem-per-cpu=1G
#SBATCH --time=04:00:00
#SBATCH --output=qcasim-sims_%A_%a.out
set -euo pipefail

LIST=${1:?usage: sbatch --array=0-<N-1> qcasim-sims.sbatch <list of .qcd files> [truth options]}
shift
HPC_DIR=${QCASIM_HPC_DIR:-${SLURM_SUBMIT_DIR:-$PWD}}
if [[ ! -f $HPC_DIR/qcasim-env.sh ]]; then
    echo "set QCASIM_HPC_DIR to the hpc/frida folder (or submit from it)" >&2
    exit 1
fi
# shellcheck source=qcasim-env.sh
source "$HPC_DIR/qcasim-env.sh"
BIN=$(qcasim_binary)
export BIN

TASK=${SLURM_ARRAY_TASK_ID:-0}
COUNT=${SLURM_ARRAY_TASK_COUNT:-1}
JOBS=${SLURM_CPUS_PER_TASK:-1}
TRUTH_OPTIONS=("$@")
export QCASIM_TRUTH=${QCASIM_TRUTH:-0}

simulate() {
    local design=$1
    shift
    local result=${design%.*}.qcs
    if [[ ! -f $result ]]; then
        "$BIN" sim "$design" -o "$result.tmp" >/dev/null && mv "$result.tmp" "$result"
    fi
    if [[ $QCASIM_TRUTH == 1 ]]; then
        "$BIN" truth "$result" "$@" > "${design%.*}.truth.txt"
    fi
    echo "done $design"
}
export -f simulate

awk -v task="$TASK" -v count="$COUNT" 'NF && (NR - 1) % count == task' "$LIST" \
    | xargs -r -d '\n' -P "$JOBS" -I{} bash -c 'simulate "$@"' _ {} "${TRUTH_OPTIONS[@]}"
