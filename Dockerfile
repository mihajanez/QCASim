# Container image with a statically linked qca-sim, e.g. for HPC clusters
# that run jobs in containers (Enroot/Pyxis on FRIDA, Apptainer, Docker).
#
#   docker build -t qcasim .
#   docker run --rm -v "$PWD:/work" -w /work qcasim qca-sim robustness plan design.qcd sweep.json
#
# Release images are published as ghcr.io/mihajanez/qcasim:<version>.
# Debian-based images: Slurm's task prolog runs inside Pyxis containers and
# needs bash, which Alpine images lack.
FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY qca-core qca-core
COPY qca-sim qca-sim
RUN RUSTFLAGS="-C target-feature=+crt-static" cargo build --release --locked -p qca-sim \
      --target "$(uname -m)-unknown-linux-gnu" \
 && cp "target/$(uname -m)-unknown-linux-gnu/release/qca-sim" /qca-sim \
 && strip /qca-sim

FROM debian:bookworm-slim
COPY --from=build /qca-sim /usr/local/bin/qca-sim
# Slurm helper scripts, so that the image alone is enough on a cluster.
COPY hpc /opt/qcasim/hpc
ENV QCASIM_HPC=/opt/qcasim/hpc
CMD ["qca-sim", "--help"]
