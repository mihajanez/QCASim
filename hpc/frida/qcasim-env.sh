# shellcheck shell=bash
# Common settings of the QCASim Slurm scripts; sourced, not executed.
#
# qca-sim is taken from one of two places:
#   * a statically linked binary (default): $QCASIM_BIN, or
#     ~/.local/bin/qca-sim-<arch> as installed by install.sh. It runs directly
#     on the compute nodes, whatever their Linux distribution.
#   * a container image, when QCASIM_IMAGE is set, e.g.
#       QCASIM_IMAGE=$WORK/images/qcasim.sqsh            (imported once, fastest)
#       QCASIM_IMAGE=ghcr.io#mihajanez/qcasim:latest     (pulled by every task)
#     The tasks then run qca-sim through Pyxis/Enroot (srun --container-image).

QCASIM_REPO=${QCASIM_REPO:-mihajanez/QCASim}
QCASIM_PREFIX=${QCASIM_PREFIX:-$HOME/.local}

qcasim_binary() {
    local arch
    arch=$(uname -m)
    if [[ -n ${QCASIM_BIN:-} ]]; then
        echo "$QCASIM_BIN"
    elif [[ -x $QCASIM_PREFIX/bin/qca-sim-$arch ]]; then
        echo "$QCASIM_PREFIX/bin/qca-sim-$arch"
    elif [[ -x $QCASIM_PREFIX/bin/qca-sim ]]; then
        echo "$QCASIM_PREFIX/bin/qca-sim"
    elif command -v qca-sim >/dev/null 2>&1; then
        command -v qca-sim
    else
        echo "error: qca-sim not found for $arch; run install.sh or set QCASIM_BIN" >&2
        return 1
    fi
}

# qca-sim on the current node, without Slurm (for light work on the login
# node, such as `robustness plan` and `robustness summary`).
qcasim_local() {
    local bin
    bin=$(qcasim_binary) || return 1
    "$bin" "$@"
}

# qca-sim as a job step of the current allocation. $1 is the directory the
# step needs (mounted into the container); the remaining arguments go to qca-sim.
qcasim_step() {
    local dir=$1
    shift
    if [[ -n ${QCASIM_IMAGE:-} ]]; then
        srun --ntasks=1 --cpus-per-task="${SLURM_CPUS_PER_TASK:-1}" \
            --container-image="$QCASIM_IMAGE" \
            --container-mounts="$dir:$dir" \
            --container-workdir="$dir" \
            qca-sim "$@"
    else
        local bin
        bin=$(qcasim_binary) || return 1
        srun --ntasks=1 --cpus-per-task="${SLURM_CPUS_PER_TASK:-1}" "$bin" "$@"
    fi
}
