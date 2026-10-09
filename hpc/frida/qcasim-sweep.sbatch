#!/usr/bin/env bash
# One task of a robustness sweep job array: simulates slice
# $SLURM_ARRAY_TASK_ID/$QCASIM_TASKS of the sweep in the run directory $1.
# Submitted by submit-sweep.sh, which sets the resources on the command line;
# the defaults below only apply to a manual `sbatch --array=...`.
#SBATCH --job-name=qcasim-sweep
#SBATCH --cpus-per-task=16
#SBATCH --mem-per-cpu=1G
#SBATCH --time=04:00:00
set -euo pipefail

RUN=${1:?usage: sbatch --array=0-<N-1> qcasim-sweep.sbatch <run directory>}
# shellcheck source=/dev/null
source "$RUN/sweep.env"
# shellcheck source=qcasim-env.sh
source "$QCASIM_HPC_DIR/qcasim-env.sh"

VARIANTS=(--no-variants)
if [[ ${QCASIM_KEEP_VARIANTS:-0} == 1 ]]; then
    VARIANTS=(--variants-dir "$RUN/variants")
fi

echo "task $SLURM_ARRAY_TASK_ID of $QCASIM_TASKS on $(hostname), $SLURM_CPUS_PER_TASK CPUs"
# A task that was interrupted (time limit, node failure, scancel) continues
# from its checkpoint when it is submitted again; a finished task exits at once.
qcasim_step "$RUN" robustness run "$RUN/design.qcd" "$RUN/config.json" \
    --output "$RUN/parts/run.json" \
    --slice "$SLURM_ARRAY_TASK_ID/$QCASIM_TASKS" \
    --threads "$SLURM_CPUS_PER_TASK" \
    --name "$QCASIM_NAME" \
    --progress-interval 60 \
    "${VARIANTS[@]}"
