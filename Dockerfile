# Container image with a statically linked qca-sim, e.g. for HPC clusters
# that run jobs in containers (Enroot/Pyxis on FRIDA, Apptainer, Docker).
#
#   docker build -t qcasim .
#   docker run --rm -v "$PWD:/work" -w /work qcasim qca-sim robustness plan design.qcd sweep.json
#
# Release images are published as ghcr.io/mihajanez/qcasim:<version>.
FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY qca-core qca-core
COPY qca-sim qca-sim
RUN cargo build --release --locked -p qca-sim \
 && strip target/release/qca-sim

FROM alpine:3.22
COPY --from=build /src/target/release/qca-sim /usr/local/bin/qca-sim
# Slurm helper scripts, so that the image alone is enough on a cluster.
COPY hpc /opt/qcasim/hpc
ENV QCASIM_HPC=/opt/qcasim/hpc
CMD ["qca-sim", "--help"]
