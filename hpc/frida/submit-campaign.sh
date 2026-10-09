#!/usr/bin/env bash
# Submits a campaign: several robustness sweeps described by a table, each as
# its own job array and merge job, plus a final job that collects one summary
# line per sweep in <campaign directory>/summary.tsv.
#
#   submit-campaign.sh [options] <campaign.tsv> [-- submit-sweep.sh options]
#
# campaign.tsv has one sweep per line (tab or space separated, '#' comments):
#
#   # name          design                     configuration         [submit-sweep.sh options]
#   line-geometry   designs/line.qcd           sweeps/geometry.json  -n 8
#   majority-o2     designs/majority.qcd       sweeps/offset-o2.json -c 32 -t 08:00:00
#
# Relative paths are relative to the directory of campaign.tsv.
set -euo pipefail

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=qcasim-env.sh
source "$HERE/qcasim-env.sh"

usage() {
    sed -n '2,16p' "$0"
    cat <<EOF

options:
  --dir DIR      campaign directory [\$QCASIM_RUNS/<campaign name>-<date>]
  --dry-run      print the sbatch commands instead of running them
EOF
}

TABLE=""
CAMPAIGN_DIR=""
DRY=()
COMMON=()
while [[ $# -gt 0 ]]; do
    case $1 in
        --dir) CAMPAIGN_DIR=$2; shift 2 ;;
        --dry-run) DRY=(--dry-run); shift ;;
        -h|--help) usage; exit 0 ;;
        --) shift; COMMON=("$@"); break ;;
        *) TABLE=$1; shift ;;
    esac
done
[[ -n $TABLE && -f $TABLE ]] || { usage >&2; exit 2; }
TABLE_DIR=$(cd "$(dirname "$TABLE")" && pwd)
NAME=$(basename "${TABLE%.*}")
RUNS_DIR=${QCASIM_RUNS:-${WORK:-$HOME}/qcasim-runs}
CAMPAIGN_DIR=${CAMPAIGN_DIR:-$RUNS_DIR/$NAME-$(date +%Y%m%d-%H%M%S)}
mkdir -p "$CAMPAIGN_DIR/logs"
CAMPAIGN_DIR=$(cd "$CAMPAIGN_DIR" && pwd)
cp "$TABLE" "$CAMPAIGN_DIR/campaign.tsv"

abs() { [[ $1 == /* ]] && echo "$1" || echo "$TABLE_DIR/$1"; }

MERGES=()
RUNS=()
printf "name\trun_dir\tsweep_job\tmerge_job\n" > "$CAMPAIGN_DIR/jobs.tsv"
while read -r -a fields || [[ ${#fields[@]} -gt 0 ]]; do
    [[ ${#fields[@]} -eq 0 || ${fields[0]} == \#* ]] && continue
    [[ ${#fields[@]} -ge 3 ]] || { echo "bad line: ${fields[*]}" >&2; exit 1; }
    sweep=${fields[0]}
    design=$(abs "${fields[1]}")
    config=$(abs "${fields[2]}")
    options=("${fields[@]:3}")
    echo "== $sweep"
    out=$("$HERE/submit-sweep.sh" "${DRY[@]}" "${COMMON[@]}" "${options[@]}" \
        --name "$sweep" --run-dir "$CAMPAIGN_DIR/$sweep" "$design" "$config" </dev/null)
    echo "$out" | grep -v '^SUBMITTED'
    read -r _ run sweep_job merge_job < <(echo "$out" | grep '^SUBMITTED')
    printf "%s\t%s\t%s\t%s\n" "$sweep" "$run" "$sweep_job" "$merge_job" >> "$CAMPAIGN_DIR/jobs.tsv"
    MERGES+=("$merge_job")
    RUNS+=("$run/run.json")
done < "$TABLE"
[[ ${#MERGES[@]} -gt 0 ]] || { echo "no sweeps in $TABLE" >&2; exit 1; }

# Collect the summaries once every merge job has ended.
cat > "$CAMPAIGN_DIR/collect.sh" <<EOF
#!/usr/bin/env bash
set -uo pipefail
source $(printf '%q' "$HERE/qcasim-env.sh")
QCASIM_IMAGE=$(printf '%q' "${QCASIM_IMAGE:-}")
QCASIM_BIN=$(printf '%q' "${QCASIM_BIN:-}")
cd $(printf '%q' "$CAMPAIGN_DIR")
present=()
for f in $(printf '%q ' "${RUNS[@]}"); do
    if [[ -f \$f ]]; then present+=("\$f"); else echo "missing \$f (see its merge log)" >&2; fi
done
qcasim_step "$CAMPAIGN_DIR" robustness summary --header "\${present[@]}" > summary.tsv
column -t -s \$'\\t' summary.tsv 2>/dev/null || cat summary.tsv
EOF
chmod +x "$CAMPAIGN_DIR/collect.sh"
DEPS=$(IFS=:; echo "${MERGES[*]}")
PARTITION_OPT=()
for ((i = 0; i < ${#COMMON[@]}; i++)); do
    case ${COMMON[i]} in
        -p|--partition) PARTITION_OPT=(--partition="${COMMON[i+1]}") ;;
        --partition=*) PARTITION_OPT=("${COMMON[i]}") ;;
    esac
done
[[ ${#PARTITION_OPT[@]} -eq 0 ]] && PARTITION_OPT=(--partition="${QCASIM_PARTITION:-amd}")
if [[ ${#DRY[@]} -gt 0 ]]; then
    echo "sbatch --dependency=afterany:$DEPS ... $CAMPAIGN_DIR/collect.sh"
else
    COLLECT=$(sbatch --parsable "${PARTITION_OPT[@]}" --dependency="afterany:$DEPS" \
        --job-name="qcasim-$NAME-collect" -c 1 --mem=2G -t 00:15:00 \
        --chdir="$CAMPAIGN_DIR" --output="$CAMPAIGN_DIR/logs/collect_%j.out" \
        "$CAMPAIGN_DIR/collect.sh" | cut -d';' -f1)
    echo "collect job: $COLLECT"
fi
echo "campaign directory: $CAMPAIGN_DIR (jobs.tsv lists the run directories; summary.tsv appears when all sweeps are merged)"
