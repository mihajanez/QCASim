#!/usr/bin/env bash
# Merges the partial run files of the sweep in run directory $1 into
# $1/run.json once all tasks of its job array have ended (afterany), and
# appends the result to the campaign summary, if there is one. If tasks
# failed, it lists the missing points; `submit-sweep.sh --resume <run dir>`
# then reruns only the unfinished tasks.
#SBATCH --job-name=qcasim-merge
#SBATCH --cpus-per-task=1
#SBATCH --mem=4G
#SBATCH --time=00:30:00
set -euo pipefail

RUN=${1:?usage: sbatch qcasim-merge.sbatch <run directory>}
# shellcheck source=/dev/null
source "$RUN/sweep.env"
# shellcheck source=qcasim-env.sh
source "$QCASIM_HPC_DIR/qcasim-env.sh"

qcasim_step "$RUN" robustness merge "$RUN/parts" --output "$RUN/run.json" --name "$QCASIM_NAME"
qcasim_step "$RUN" robustness summary "$RUN/run.json" --header > "$RUN/summary.tsv"
cat "$RUN/summary.tsv"
