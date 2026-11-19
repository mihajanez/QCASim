# QCASim
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](./LICENSE)
[![Build and Test](https://github.com/mihajanez/QCASim/actions/workflows/build-test.yml/badge.svg)](https://github.com/mihajanez/QCASim/actions/workflows/build-test.yml)

QCASim is the simulation core and command-line tool of the QCAForge framework for binary and ternary quantum-dot cellular automata (QCA). It consists of the library **QCACore** (`qca-core`), which implements the data model, the open file formats, the simulation models and the robustness engine, and the command-line tool **QCASim** (`qca-sim`) for scripted, reproducible and large-scale simulation studies. The desktop application [QCAForge](https://github.com/mihajanez/QCAForge) uses the same library.

## Features

- **Simulation models**: the bistable approximation (binary cells, with a heuristic extension for eight-dot cells) and the quantum-mechanical ICHA model (intercellular Hartree approximation) for cells with any number of dots and electrons; the ICHA cell update order is selectable (sequential, reverse, random or simultaneous). Further models are added through a common model interface.
- **File formats**: `.qcd` (QCA design: layout, cell architectures and all simulation settings) and `.qcs` (self-contained simulation results with metadata)
- **Truth tables**: logic states of the input and output cells per input combination, with clock, logical and value thresholds and per-cell clock delays
- **Robustness sweeps**: `qca-sim robustness` sweeps one or two exported geometry, model or clock parameters, simulates all variants in parallel and scores them against the expected behaviour; sweeps can be split into slices for HPC job arrays, checkpointed and merged
- **HPC workflow**: Slurm scripts and a guide for the FRIDA cluster (UL FRI) in [`hpc/frida`](hpc/frida/README.md)
- **Python scripts**: generation of design variants, batch simulation and analysis of results in [`scripts/`](scripts/)

## Installation

```bash
git clone https://github.com/mihajanez/QCASim.git
cd QCASim
cargo build --release
```

Every [release](https://github.com/mihajanez/QCASim/releases) provides packaged binaries for Linux x86_64 and Windows, statically linked Linux binaries for x86_64 and aarch64 (`qca-sim-linux-x86_64`, `qca-sim-linux-aarch64`), which run on any Linux system including HPC compute nodes, and the container image `ghcr.io/mihajanez/qcasim`.

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
- `examples/`: example designs (wire with a simulation result, flip-flops); the elementary ternary structures are in the `examples/` folder of QCAForge
- `scripts/`: Python analysis tools and batch processing utilities

## License

This project is licensed under the MIT License. See the [LICENSE](LICENSE) file for details.
