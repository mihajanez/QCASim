# QCASim
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](./LICENSE)
[![Build and Test](https://github.com/mihajanez/QCASim/actions/workflows/build-test.yml/badge.svg)](https://github.com/mihajanez/QCASim/actions/workflows/build-test.yml)

A Quantum Cellular Automata (QCA) simulation framework for academic research, providing tools to model and analyze quantum dot cellular automata circuits.

## Features

- **Simulation engine**: Supports built-in bistable and ICHA model with option to use custom models as well.
- **File Formats**: Defines and uses `.qcd` (QCA Design) and `.qcs` (QCA Simulation) file formats
- **Truth Table Analysis**: Generate and analyze logic truth tables from simulation results
- **CLI Interface**: Command-line tools for simulation and analysis
- **Robustness sweeps**: the QCAForge robustness engine as a command (`qca-sim robustness`), splittable into slices for HPC job arrays
- **HPC workflow**: Slurm scripts and a guide for the FRIDA cluster (UL FRI) in [`hpc/frida`](hpc/frida/README.md)
- **Example Designs**: Includes wire, inverter, majority gate, and memory cell designs
- **Analysis**: Interactive analysis scripts for simulation data

## Installation

```bash
git clone https://github.com/mihajanez/QCASim.git
cd QCASim
cargo build --release
```

Prebuilt binaries (statically linked for Linux x86_64/aarch64, Windows, macOS)
are attached to the [releases](https://github.com/mihajanez/QCASim/releases);
a container image is published as `ghcr.io/mihajanez/qcasim`.

## Usage

### Run Simulation

```bash
qca-sim sim examples/line.qcd
```

### Truth table of a simulation

```bash
qca-sim truth examples/line.qcs
```

### Robustness sweeps

```bash
# check a sweep and see how it splits into 8 slices
qca-sim robustness plan examples/line.qcd hpc/frida/examples/line-geometry.json --tasks 8
# run it (all cores), or one slice of it
qca-sim robustness run examples/line.qcd hpc/frida/examples/line-geometry.json -o run.json
qca-sim robustness run examples/line.qcd hpc/frida/examples/line-geometry.json -o parts/run.json --slice 3/8
# merge the slices and print statistics
qca-sim robustness merge parts -o run.json
qca-sim robustness summary --header run.json
```

The run file opens in QCAForge (Robustness → Open results). Inside a Slurm
job array the slice and the thread count are taken from
`SLURM_ARRAY_TASK_ID`/`SLURM_ARRAY_TASK_COUNT` and `SLURM_CPUS_PER_TASK`;
every finished point is checkpointed, so an interrupted task continues where
it stopped. See [`hpc/frida/README.md`](hpc/frida/README.md) for the complete
cluster workflow (job arrays, campaigns of many sweeps, containers).

### Analysis

Use the Jupyter notebook `scripts/analysis.ipynb` for interactive analysis of simulation results and visualization.

## File Formats

- **`.qcd`**: QCA design files containing circuit layout, cell positions, and simulation parameters
- **`.qcs`**: QCA simulation files containing simulation results and metadata

## Project Structure

- `qca-core/`: Core simulation engine and data structures, truth tables and the robustness engine (`analysis::robustness`)
- `qca-sim/`: CLI application for running and analyzing simulations
- `hpc/`: Slurm workflow for HPC clusters (FRIDA)
- `examples/`: Example QCA designs
- `scripts/`: Python analysis tools and batch processing utilities

## Status

This project is stable and under active development for academic research applications.

## License

This project is licensed under the MIT License. See the [LICENSE](LICENSE) file for details.
