# Running QCASim on the FRIDA HPC cluster

This folder contains everything needed to run large QCA simulation studies on
[FRIDA](https://docs.rdc.si/), the Slurm cluster of the University of
Ljubljana, Faculty of Computer and Information Science (UL FRI). The scripts
use only standard Slurm features, so they also work on other Slurm clusters
(set `QCASIM_PARTITION` and the other variables below).

The workload is a **robustness sweep**: a nominal design (`.qcd`) and a sweep
configuration (`.json`) define a one- or two-dimensional grid of design
variants (cell size, dot radius, dot diameter, layer height, displacement of
a labelled cell, or any numeric model/clock parameter). Every variant is
simulated and its truth table is scored against the expected behaviour. The
variants are independent, so the sweep is split into *slices* that run as
the tasks of a Slurm **job array**, and a dependent job **merges** the
partial results into one run file for the QCAForge Robustness view.

```
workstation (QCAForge)          FRIDA login node                 FRIDA compute nodes
──────────────────────          ────────────────                 ───────────────────
Robustness view                 submit-sweep.sh                  array task k of N:
 → "Export for cluster"  ─scp→   ├ qca-sim robustness plan        qca-sim robustness run --slice k/N
   design.qcd + sweep.json       ├ sbatch --array=0-(N-1) ─────→   (threads = CPUs of the task,
                                 └ sbatch -d afterany ──┐          checkpoint after every point)
                                                        └──────→ merge job:
Robustness view                                                   qca-sim robustness merge → run.json
 ← "Open results"        ←scp─  run.json, summary.tsv             qca-sim robustness summary
```

| File | Purpose |
|---|---|
| `install.sh` | installs `qca-sim` (static release binaries, a source build, or the container image) |
| `submit-sweep.sh` | submits one sweep (job array + merge job); `--resume` reruns unfinished tasks |
| `submit-campaign.sh` | submits many sweeps listed in a table, plus a job that collects their summaries |
| `qcasim-sweep.sbatch`, `qcasim-merge.sbatch` | the job scripts used by `submit-sweep.sh` |
| `qcasim-sims.sbatch` | plain `qca-sim sim` (and `truth`) for a list of designs, e.g. from `scripts/gen_designs.py` |
| `qcasim-env.sh` | common settings (where `qca-sim` comes from, container mode) |
| `examples/` | example sweep configurations and campaign |

## 1. Access

* FRIDA accounts are given to UL FRI employees, and to students under an
  employee's supervision; the supervisor requests access at
  <frida@rdc.si> (research lab, project or thesis topic, duration).
  Accounts without jobs for six months are archived.
* Login is with multi-factor authentication through Teleport. Install the
  `tsh` client, then

  ```bash
  tsh --proxy=rdc.si --user=<username> login
  tsh config >> ~/.ssh/config       # once; afterwards plain ssh/scp work
  ssh <username>@login-frida.rdc.si
  ```

  (see <https://docs.rdc.si/FRIDA/access/> for passwordless login and the
  Windows/VS Code set-up).
* House rules: the login node is only for light work (editing, `git`,
  `submit-*.sh`, `qca-sim robustness plan|merge|summary`); all simulations
  run through Slurm. Interactive jobs (`srun --pty`, `salloc`) are only
  allowed on the `dev` partition and not for production runs. There are no
  backups.

## 2. One-time set-up on the login node

```bash
git clone https://github.com/mihajanez/QCASim.git ~/QCASim
~/QCASim/hpc/frida/install.sh            # downloads qca-sim-x86_64 and qca-sim-aarch64 to ~/.local/bin
```

The release binaries are statically linked, so they run on every FRIDA node
(x86_64 nodes and the aarch64 Grace-Hopper nodes `gh[1-2]`) without modules
or containers. Alternatives:

* `install.sh --version v0.2.0` pins a release (recommended for a study, so
  that every run uses the same QCACore version; the version is recorded in
  every run directory and in every `.qcs` file);
* `install.sh --build` compiles the checked-out source in a Rust container
  on the `dev` partition (for unreleased changes);
* `install.sh --image` imports the container image
  `ghcr.io/mihajanez/qcasim` once into `$WORK/images/qcasim-<version>.sqsh`.
  `export QCASIM_IMAGE=<that file>` makes all jobs run `qca-sim` through
  Pyxis/Enroot (`srun --container-image`) instead of the binary.

Store run data in the shared workspace of your Slurm account,
`/shared/workspace/<account>` (`$WORK` inside jobs), rather than in `$HOME`:

```bash
export QCASIM_RUNS=/shared/workspace/<account>/$USER/qcasim-runs   # e.g. in ~/.bashrc
```

## 3. Prepare a sweep

The easiest way is the QCAForge **Robustness** view: set up the sweep as for a
local run and click **Export for cluster**. It writes a folder with the
nominal design (`design.qcd`), the configuration (`sweep.json`) and a
`submit.sh` that calls `submit-sweep.sh` with these files. Copy the folder to
FRIDA:

```bash
scp -r my-sweep login-frida.rdc.si:/shared/workspace/<account>/$USER/
```

A configuration can also be written by hand. Values are a list or an
inclusive `"start:stop:step"` range:

```json
{
  "x_axis": { "parameter": "geometry.cell_size",  "values": "50:150:3.125" },
  "y_axis": { "parameter": "geometry.dot_radius", "values": "14:24:0.5" },
  "expected_behavior": "wire",
  "cell_clock_delays": {},
  "thresholds": { "clock": 0.05, "logical": 0.05, "value": 0.8 }
}
```

| Field | Meaning |
|---|---|
| `x_axis`, `y_axis` (optional) | swept parameter and its values. Parameters: `geometry.cell_size`, `geometry.dot_radius`, `geometry.dot_diameter`, `geometry.layer_z:<layer>`, `geometry.cell_offset_x:<label>`, `geometry.cell_offset_y:<label>`, `model.<key>`, `clock.<key>` (e.g. `model.relative_permitivity`, `clock.amplitude_max`) |
| `expected_behavior` | `reference` (outputs must match the nominal design), `wire`, `inverter`, `majority`, `memory_cell`, `flipflop1`, `ternary_flipflop` |
| `cell_clock_delays` | clock-cycle delay of output cells, by label, as `qca-sim truth -d` |
| `thresholds` | truth-table thresholds (defaults of `qca-sim truth`) |
| `output_dir` | keep every variant's `.qcd`/`.qcs` here (usually off on the cluster; see `--keep-variants`) |

Check a configuration and see how it splits (no simulation; fine on the login node):

```bash
qca-sim-x86_64 robustness plan design.qcd sweep.json --tasks 16
# 693 points: geometry.cell_size x 33, geometry.dot_radius x 21
# 16 tasks: 43 to 44 points per task
```

## 4. Submit

```bash
cd /shared/workspace/<account>/$USER/my-sweep
~/QCASim/hpc/frida/submit-sweep.sh design.qcd sweep.json            # or ./submit.sh
# my-sweep: 693 points in 11 tasks x 16 CPUs on partition amd -> .../qcasim-runs/my-sweep-20261009-101500
# sweep job array: 4711
# merge job: 4712
```

Options (defaults in brackets, also settable as environment variables
`QCASIM_CPUS`, `QCASIM_PARTITION`, `QCASIM_TIME`, `QCASIM_MEM_PER_CPU`, `QCASIM_ACCOUNT`):

| Option | |
|---|---|
| `-n, --tasks N` | array tasks (slices); default about four points per CPU |
| `-c, --cpus N` | CPUs = worker threads per task [16] |
| `-p, --partition P` | [amd]; FRIDA's `amd` node (`api`, 384 vCPUs) is meant for CPU-intensive jobs; `frida` has more nodes (GPU nodes with 112–256 vCPUs each, 7-day limit); `dev` for short tests (12 h) |
| `-t, --time T` | time limit per task [04:00:00] |
| `-m, --mem-per-cpu M` | [1G]; one simulation needs well under 100 MB |
| `--throttle K` | at most K tasks at the same time (`--array=…%K`), to leave room for others |
| `--keep-variants` | also keep every variant's `.qcd`/`.qcs` (can be many GB) |
| `-- …` | further `sbatch` options, e.g. `-- --mail-type=END --mail-user=me@fri.uni-lj.si` |

Each task writes `parts/run.part-<k>-of-<N>.json`; the merge job writes
`run.json` and `summary.tsv` into the run directory, which also keeps a copy
of the design and configuration, `sweep.env` (all settings), the `qca-sim`
version and the job logs.

**Sizing.** The points of a slice are interleaved over the grid (task k takes
points k, k+N, k+2N, …), so the slowly converging variants near operating
boundaries spread evenly over the tasks. A task's run time is roughly
(points per task / CPUs) × (time per simulation); measure the time per
simulation with a short run on `dev` first (`run.json` stores the simulation
time of every point). Simulations of the wire take seconds, majority gates
tens of seconds, and one simulation of the 66-cell ternary flip-flop with
reset (18 input vectors) about half an hour on one core.

## 5. Monitor, recover, collect

```bash
squeue --me                                     # or `frida` for an overview
tail -f <run>/logs/sweep_4711_3.out             # progress lines with an ETA
sacct -j 4711 --format=JobID,State,Elapsed,MaxRSS
```

Every task appends each finished point to a checkpoint
(`parts/*.checkpoint.jsonl`). If tasks fail, hit the time limit or are
cancelled, the merge job reports the missing grid points; then

```bash
~/QCASim/hpc/frida/submit-sweep.sh --resume <run directory> [-t 08:00:00]
```

resubmits only the unfinished tasks, which continue from their checkpoints,
followed by a new merge. Results do not depend on how a sweep is sliced or
resumed: the simulation is deterministic, so a merged run is identical to a
single-process run of the same sweep.

Copy the results back and open them in QCAForge (Robustness → **Open
results**):

```bash
scp login-frida.rdc.si:<run directory>/run.json .
```

The partial `parts/*.json` files can also be opened together in QCAForge,
which merges them (useful to look at a sweep while it is still running).

## 6. Campaigns (complex experiments)

A study usually consists of many sweeps: several circuits × several
parameter pairs, displacement sweeps of every cell, defect variants, …
Describe them in a table, one sweep per line (relative paths are relative to
the table; extra columns are `submit-sweep.sh` options):

```
# name               design                  configuration          options
wire-geometry        designs/line.qcd        sweeps/geometry.json   -n 8
majority-geometry    designs/majority.qcd    sweeps/geometry.json   -c 32
majority-offset-M    designs/majority.qcd    sweeps/offset-M.json   -c 32 -t 08:00:00
flipflop-geometry    designs/flipflop.qcd    sweeps/ff-geometry.json -c 64 -t 1-00:00:00
```

```bash
~/QCASim/hpc/frida/submit-campaign.sh study.tsv [-- -p frida --throttle 20]
```

Every sweep gets its own run directory below the campaign directory, and a
final job writes `summary.tsv` with one line per sweep (points, missing,
errors, fully correct variants, mean and minimum accuracy, simulation
core-hours, wall time). `jobs.tsv` lists the job ids;
`submit-sweep.sh --resume <campaign>/<sweep>` repairs a single sweep.
`examples/example-campaign.tsv` is a small working example.

## 7. Plain simulations

For workflows built on the Python scripts (`scripts/gen_designs.py` →
simulate → `scripts/analyze_truth.py`), `qcasim-sims.sbatch` simulates a list
of design files:

```bash
python scripts/gen_designs.py ...                  # writes designs/*.qcd
ls $PWD/designs/*.qcd > designs.txt
export QCASIM_HPC_DIR=~/QCASim/hpc/frida
sbatch -p amd --array=0-7 -c 32 $QCASIM_HPC_DIR/qcasim-sims.sbatch designs.txt
```

Each of the 8 tasks takes every 8th design and runs 32 simulations at a time;
existing `.qcs` files are skipped, so the same command resumes an interrupted
batch. With `export QCASIM_TRUTH=1` (and `qca-sim truth` options after the
list, e.g. `-d Q:1`) the truth table of each result is written next to it.

A single simulation for testing:

```bash
srun -p dev -c 1 --mem=2G -t 00:30:00 ~/.local/bin/qca-sim-x86_64 sim design.qcd
```

## 8. Using another cluster

Nothing in the scripts is FRIDA-specific except the defaults. Set
`QCASIM_PARTITION`, optionally `QCASIM_ACCOUNT`, and either `QCASIM_BIN` (path
to a `qca-sim` binary) or `QCASIM_IMAGE` (a container image for Pyxis; for
Apptainer, build a binary with `install.sh --build` or `cargo build
--release -p qca-sim`).
