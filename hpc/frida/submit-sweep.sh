#!/usr/bin/env bash
# Submits a robustness sweep to Slurm (FRIDA) as a job array plus a merge job.
#
#   submit-sweep.sh [options] <design.qcd> <sweep.json>
#   submit-sweep.sh --resume <run directory>
#
# Creates a run directory with a copy of the design and the configuration,
# splits the sweep into N slices (one array task each), and submits a merge
# job that writes <run directory>/run.json when all tasks have ended. Open
# run.json in QCAForge (Robustness -> Open results).
set -euo pipefail

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=qcasim-env.sh
source "$HERE/qcasim-env.sh"

usage() {
    cat <<EOF
usage: $(basename "$0") [options] <design.qcd> <sweep.json>
       $(basename "$0") [options] --resume <run directory>

  -n, --tasks N         number of array tasks (slices); default: about
                        4 points per CPU, at most ${QCASIM_MAX_TASKS:-1000}
  -c, --cpus N          CPUs (worker threads) per task        [${QCASIM_CPUS:-16}]
  -p, --partition P     Slurm partition                       [${QCASIM_PARTITION:-amd}]
  -t, --time T          time limit per task                   [${QCASIM_TIME:-04:00:00}]
  -m, --mem-per-cpu M   memory per CPU                        [${QCASIM_MEM_PER_CPU:-1G}]
  -A, --account A       Slurm account
      --throttle K      run at most K tasks at the same time (--array=...%K)
      --name NAME       name of the run (default: design file name)
      --runs-dir DIR    parent of the run directory   [\$QCASIM_RUNS, \$WORK/qcasim-runs or ~/qcasim-runs]
      --run-dir DIR     use exactly this run directory
      --keep-variants   keep the .qcd/.qcs file of every variant (large!)
      --resume DIR      resubmit the unfinished tasks of an earlier run
      --dry-run         print the sbatch commands instead of running them
  -- ...                further options passed to both sbatch calls
EOF
}

TASKS=""
CPUS=${QCASIM_CPUS:-16}
PARTITION=${QCASIM_PARTITION:-amd}
TIME=${QCASIM_TIME:-04:00:00}
MEM_PER_CPU=${QCASIM_MEM_PER_CPU:-1G}
ACCOUNT=${QCASIM_ACCOUNT:-}
THROTTLE=""
NAME=""
RUNS_DIR=${QCASIM_RUNS:-${WORK:-$HOME}/qcasim-runs}
RUN=""
KEEP_VARIANTS=0
RESUME=""
DRY_RUN=0
EXTRA=()
POSITIONAL=()

while [[ $# -gt 0 ]]; do
    case $1 in
        -n|--tasks) TASKS=$2; shift 2 ;;
        -c|--cpus) CPUS=$2; shift 2 ;;
        -p|--partition) PARTITION=$2; shift 2 ;;
        -t|--time) TIME=$2; shift 2 ;;
        -m|--mem-per-cpu) MEM_PER_CPU=$2; shift 2 ;;
        -A|--account) ACCOUNT=$2; shift 2 ;;
        --throttle) THROTTLE=$2; shift 2 ;;
        --name) NAME=$2; shift 2 ;;
        --runs-dir) RUNS_DIR=$2; shift 2 ;;
        --run-dir) RUN=$2; shift 2 ;;
        --keep-variants) KEEP_VARIANTS=1; shift ;;
        --resume) RESUME=$2; shift 2 ;;
        --dry-run) DRY_RUN=1; shift ;;
        -h|--help) usage; exit 0 ;;
        --) shift; EXTRA=("$@"); break ;;
        -*) echo "unknown option $1" >&2; usage >&2; exit 2 ;;
        *) POSITIONAL+=("$1"); shift ;;
    esac
done

if [[ -n $RESUME ]]; then
    RUN=$(cd "$RESUME" && pwd)
    # shellcheck source=/dev/null
    source "$RUN/sweep.env"
    TASKS=$QCASIM_TASKS
    NAME=$QCASIM_NAME
    KEEP_VARIANTS=${QCASIM_KEEP_VARIANTS:-0}
    TODO=()
    last=$((TASKS - 1))
    width=${#last}  # qca-sim pads the slice index to the width of the last index
    for ((i = 0; i < TASKS; i++)); do
        part=$(printf "%s/parts/run.part-%0${width}d-of-%d.json" "$RUN" "$i" "$TASKS")
        [[ $TASKS -eq 1 ]] && part="$RUN/parts/run.json"
        [[ -f $part ]] || TODO+=("$i")
    done
    if [[ ${#TODO[@]} -eq 0 ]]; then
        echo "all $TASKS tasks of $RUN have finished; submitting only the merge"
        ARRAY=""
    else
        ARRAY=$(IFS=,; echo "${TODO[*]}")
        echo "resubmitting ${#TODO[@]} of $TASKS tasks: $ARRAY"
    fi
else
    [[ ${#POSITIONAL[@]} -eq 2 ]] || { usage >&2; exit 2; }
    DESIGN=${POSITIONAL[0]}
    CONFIG=${POSITIONAL[1]}
    [[ -f $DESIGN ]] || { echo "no such design: $DESIGN" >&2; exit 1; }
    [[ -f $CONFIG ]] || { echo "no such configuration: $CONFIG" >&2; exit 1; }
    NAME=${NAME:-$(basename "${DESIGN%.*}")}

    # Validates the design and configuration on the login node (no simulation).
    PLAN=$(qcasim_local robustness plan "$DESIGN" "$CONFIG" --json)
    POINTS=$(sed -n 's/.*"points":\([0-9]*\).*/\1/p' <<<"$PLAN")
    if [[ -z $TASKS ]]; then
        TASKS=$(( (POINTS + 4 * CPUS - 1) / (4 * CPUS) ))
        (( TASKS > ${QCASIM_MAX_TASKS:-1000} )) && TASKS=${QCASIM_MAX_TASKS:-1000}
    fi
    (( TASKS < 1 )) && TASKS=1
    (( TASKS > POINTS )) && TASKS=$POINTS
    if (( TASKS * CPUS > 4 * POINTS )); then
        echo "note: $TASKS tasks x $CPUS CPUs for $POINTS points leaves CPUs idle; consider fewer CPUs per task" >&2
    fi

    if [[ -z $RUN ]]; then
        RUN=$RUNS_DIR/$NAME-$(date +%Y%m%d-%H%M%S)
    fi
    mkdir -p "$RUN/parts" "$RUN/logs"
    RUN=$(cd "$RUN" && pwd)
    cp "$DESIGN" "$RUN/design.qcd"
    cp "$CONFIG" "$RUN/config.json"
    cat > "$RUN/sweep.env" <<EOF
# Written by submit-sweep.sh on $(date -Iseconds)
QCASIM_NAME=$(printf '%q' "$NAME")
QCASIM_TASKS=$TASKS
QCASIM_POINTS=$POINTS
QCASIM_KEEP_VARIANTS=$KEEP_VARIANTS
QCASIM_HPC_DIR=$(printf '%q' "$HERE")
QCASIM_IMAGE=$(printf '%q' "${QCASIM_IMAGE:-}")
QCASIM_BIN=$(printf '%q' "${QCASIM_BIN:-}")
QCASIM_SOURCE_DESIGN=$(printf '%q' "$(cd "$(dirname "$DESIGN")" && pwd)/$(basename "$DESIGN")")
QCASIM_SOURCE_CONFIG=$(printf '%q' "$(cd "$(dirname "$CONFIG")" && pwd)/$(basename "$CONFIG")")
EOF
    qcasim_local --version > "$RUN/qca-sim.version" 2>/dev/null || true
    echo "$NAME: $POINTS points in $TASKS tasks x $CPUS CPUs on partition $PARTITION -> $RUN"
    ARRAY="0-$((TASKS - 1))"
fi

COMMON=(--partition="$PARTITION" --chdir="$RUN")
[[ -n $ACCOUNT ]] && COMMON+=(--account="$ACCOUNT")
COMMON+=("${EXTRA[@]}")

submit() {
    if [[ $DRY_RUN == 1 ]]; then
        echo "sbatch $*" >&2
        echo "DRYRUN$RANDOM"
    else
        sbatch --parsable "$@" | cut -d';' -f1
    fi
}

DEPENDENCY=()
if [[ -n $ARRAY ]]; then
    [[ -n $THROTTLE ]] && ARRAY="$ARRAY%$THROTTLE"
    SWEEP_JOB=$(submit "${COMMON[@]}" \
        --job-name="qcasim-$NAME" \
        --array="$ARRAY" \
        --cpus-per-task="$CPUS" \
        --mem-per-cpu="$MEM_PER_CPU" \
        --time="$TIME" \
        --output="$RUN/logs/sweep_%A_%a.out" \
        "$HERE/qcasim-sweep.sbatch" "$RUN")
    DEPENDENCY=(--dependency="afterany:$SWEEP_JOB")
    echo "sweep job array: $SWEEP_JOB"
    echo "$SWEEP_JOB" >> "$RUN/jobs.txt"
fi
MERGE_JOB=$(submit "${COMMON[@]}" "${DEPENDENCY[@]}" \
    --job-name="qcasim-$NAME-merge" \
    --output="$RUN/logs/merge_%j.out" \
    "$HERE/qcasim-merge.sbatch" "$RUN")
echo "merge job: $MERGE_JOB"
echo "$MERGE_JOB" >> "$RUN/jobs.txt"

# Machine-readable last line for submit-campaign.sh.
echo "SUBMITTED $RUN ${SWEEP_JOB:-none} $MERGE_JOB"
