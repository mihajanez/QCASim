//! `qca-sim robustness`: headless robustness sweeps for workstations and HPC
//! clusters. A sweep can be split into slices that run as the tasks of a Slurm
//! job array (the slice is taken from `SLURM_ARRAY_TASK_ID`/`_COUNT`), each
//! task checkpoints its finished points so that a requeued or resubmitted task
//! continues where it stopped, and `merge` combines the partial run files into
//! one run file for the QCAForge Robustness view.

use clap::builder::PathBufValueParser;
use clap::{value_parser, Arg, ArgAction, ArgMatches, Command};
use qca_core::analysis::robustness::{
    merge_runs, run_sweep, slice_sizes, RobustnessConfig, RobustnessProgress, RobustnessRun,
    RunSliceInfo, SweepObserver, SweepOptions, SweepPoint, SweepSlice,
};
use qca_core::design::file::QCADesign;
use serde_json::{json, Value};
use std::error::Error;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const CHECKPOINT_FORMAT: &str = "qca-sim-robustness-checkpoint";

pub fn get_robustness_subcommand() -> Command {
    let design = Arg::new("design")
        .help("Nominal design (.qcd file saved by QCAForge)")
        .value_parser(PathBufValueParser::default())
        .required(true);
    let config = Arg::new("config")
        .help("Sweep configuration (JSON; exported by the QCAForge Robustness view)")
        .value_parser(PathBufValueParser::default())
        .required(true);
    Command::new("robustness")
        .about("Robustness sweeps: simulate and score every variant of a parameter grid")
        .subcommand_required(true)
        .subcommand(
            Command::new("run")
                .about("Run a sweep, or one slice of it (e.g. one task of a Slurm job array)")
                .arg(design.clone())
                .arg(config.clone())
                .arg(
                    Arg::new("output")
                        .short('o')
                        .long("output")
                        .help("Run file to write. May contain {index} and {count}; for a slice without \
                               them, '.part-<index>-of-<count>' is inserted before the extension")
                        .value_parser(PathBufValueParser::default())
                        .required(true),
                )
                .arg(
                    Arg::new("slice")
                        .long("slice")
                        .help("Only run slice <index>/<count> (0-based), i.e. every <count>-th point. \
                               Defaults to the Slurm array task, if any")
                        .value_parser(|s: &str| s.parse::<SweepSlice>()),
                )
                .arg(
                    Arg::new("no-slurm")
                        .long("no-slurm")
                        .help("Ignore the SLURM_* environment variables")
                        .action(ArgAction::SetTrue),
                )
                .arg(
                    Arg::new("threads")
                        .short('j')
                        .long("threads")
                        .help("Worker threads (default: SLURM_CPUS_PER_TASK, the configuration, \
                               or all available cores)")
                        .value_parser(value_parser!(usize)),
                )
                .arg(
                    Arg::new("name")
                        .long("name")
                        .help("Name of the run shown in QCAForge (default: design file name)"),
                )
                .arg(
                    Arg::new("variants-dir")
                        .long("variants-dir")
                        .help("Keep the .qcd and .qcs file of every variant in this folder \
                               (overrides output_dir of the configuration)")
                        .value_parser(PathBufValueParser::default()),
                )
                .arg(
                    Arg::new("no-variants")
                        .long("no-variants")
                        .help("Do not keep variant files, even if the configuration sets output_dir")
                        .action(ArgAction::SetTrue),
                )
                .arg(
                    Arg::new("max-points")
                        .long("max-points")
                        .help("Largest accepted sweep")
                        .default_value("10000000")
                        .value_parser(value_parser!(usize)),
                )
                .arg(
                    Arg::new("progress-interval")
                        .long("progress-interval")
                        .help("Seconds between progress lines on stderr")
                        .default_value("30")
                        .value_parser(value_parser!(u64)),
                )
                .arg(
                    Arg::new("force")
                        .long("force")
                        .help("Run even if the run file already exists, and discard any checkpoint")
                        .action(ArgAction::SetTrue),
                ),
        )
        .subcommand(
            Command::new("merge")
                .about("Merge the run files of the slices of a sweep into one run file")
                .arg(
                    Arg::new("inputs")
                        .help("Run files, or folders whose *.json run files are merged")
                        .value_parser(PathBufValueParser::default())
                        .num_args(1..)
                        .required(true),
                )
                .arg(
                    Arg::new("output")
                        .short('o')
                        .long("output")
                        .help("Merged run file")
                        .value_parser(PathBufValueParser::default())
                        .required(true),
                )
                .arg(Arg::new("name").long("name").help("Name of the merged run"))
                .arg(
                    Arg::new("allow-incomplete")
                        .long("allow-incomplete")
                        .help("Write the merged file even if grid points are missing")
                        .action(ArgAction::SetTrue),
                ),
        )
        .subcommand(
            Command::new("summary")
                .about("Print one tab-separated line of statistics per run file")
                .arg(
                    Arg::new("inputs")
                        .help("Run files")
                        .value_parser(PathBufValueParser::default())
                        .num_args(1..)
                        .required(true),
                )
                .arg(
                    Arg::new("header")
                        .long("header")
                        .help("Print a header line first")
                        .action(ArgAction::SetTrue),
                ),
        )
        .subcommand(
            Command::new("plan")
                .about("Validate a design and sweep configuration and print the size of the sweep")
                .arg(design)
                .arg(config)
                .arg(
                    Arg::new("tasks")
                        .short('n')
                        .long("tasks")
                        .help("Show the number of points of each of this many slices")
                        .value_parser(value_parser!(usize)),
                )
                .arg(
                    Arg::new("json")
                        .long("json")
                        .help("Print a JSON object instead of text")
                        .action(ArgAction::SetTrue),
                ),
        )
}

pub fn run_robustness(matches: &ArgMatches) -> Result<(), Box<dyn Error>> {
    match matches.subcommand() {
        Some(("run", m)) => run(m),
        Some(("merge", m)) => merge(m),
        Some(("plan", m)) => plan(m),
        Some(("summary", m)) => summary(m),
        _ => Err("Invalid robustness command".into()),
    }
}

// ---------------------------------------------------------------------------
// Input
// ---------------------------------------------------------------------------

/// Reads a design file; returns the raw design (with GUI-only fields), the
/// parsed design and the designer properties.
fn read_design(path: &Path) -> Result<(Value, QCADesign, Value, String), Box<dyn Error>> {
    let text = fs::read_to_string(path)
        .map_err(|e| format!("Could not read {}: {}", path.display(), e))?;
    let file: Value = serde_json::from_str(&text)
        .map_err(|e| format!("{} is not a design file: {}", path.display(), e))?;
    let raw = file
        .get("design")
        .cloned()
        .ok_or_else(|| format!("{} has no design", path.display()))?;
    let design: QCADesign = serde_json::from_value(raw.clone())
        .map_err(|e| format!("Invalid design in {}: {}", path.display(), e))?;
    let designer_properties = file.get("designer_properties").cloned().unwrap_or(Value::Null);
    Ok((raw, design, designer_properties, fnv1a(text.as_bytes())))
}

/// Expands `"start:stop:step"` strings (inclusive, the syntax of the Python
/// scripts and the GUI) into value lists.
fn expand_range(value: &mut Value) -> Result<(), String> {
    let Some(text) = value.as_str() else {
        return Ok(());
    };
    let parts: Vec<f64> = text
        .split(':')
        .map(|p| p.trim().parse::<f64>())
        .collect::<Result<_, _>>()
        .map_err(|_| format!("Invalid range '{}'", text))?;
    let [start, stop, step] = parts[..] else {
        return Err(format!("Range '{}' must have the form start:stop:step", text));
    };
    if step <= 0.0 || stop < start {
        return Err(format!("Invalid range '{}'", text));
    }
    let n = ((stop - start) / step + 1e-9).floor() as usize + 1;
    if n > 100_000 {
        return Err(format!("Range '{}' has too many values", text));
    }
    *value = Value::from(
        (0..n)
            .map(|i| ((start + i as f64 * step) * 1e9).round() / 1e9)
            .collect::<Vec<f64>>(),
    );
    Ok(())
}

fn read_config(path: &Path) -> Result<RobustnessConfig, Box<dyn Error>> {
    let text = fs::read_to_string(path)
        .map_err(|e| format!("Could not read {}: {}", path.display(), e))?;
    let mut value: Value = serde_json::from_str(&text)
        .map_err(|e| format!("{} is not JSON: {}", path.display(), e))?;
    for axis in ["x_axis", "y_axis"] {
        if let Some(values) = value.get_mut(axis).and_then(|a| a.get_mut("values")) {
            expand_range(values)?;
        }
    }
    if value.get("thresholds").is_none() {
        value["thresholds"] = json!({"clock": 0.05, "logical": 0.05, "value": 0.8});
    }
    Ok(serde_json::from_value(value)
        .map_err(|e| format!("Invalid sweep configuration {}: {}", path.display(), e))?)
}

fn env_usize(name: &str) -> Option<usize> {
    std::env::var(name).ok()?.trim().parse().ok()
}

/// The slice of a Slurm job array task (`--array=0-15` → 0/16, ..., 15/16).
fn slurm_slice() -> Option<SweepSlice> {
    let id = env_usize("SLURM_ARRAY_TASK_ID")?;
    let count = env_usize("SLURM_ARRAY_TASK_COUNT")?;
    let min = env_usize("SLURM_ARRAY_TASK_MIN").unwrap_or(0);
    let step = env_usize("SLURM_ARRAY_TASK_STEP").unwrap_or(1).max(1);
    let slice = SweepSlice {
        index: id.checked_sub(min)? / step,
        count,
    };
    slice.validate().ok().map(|_| slice)
}

fn output_path(template: &Path, slice: Option<SweepSlice>) -> PathBuf {
    let text = template.to_string_lossy();
    let Some(slice) = slice else {
        return template.to_path_buf();
    };
    let width = slice.count.saturating_sub(1).to_string().len();
    let index = format!("{:0width$}", slice.index, width = width);
    if text.contains("{index}") || text.contains("{count}") {
        return PathBuf::from(
            text.replace("{index}", &index)
                .replace("{count}", &slice.count.to_string()),
        );
    }
    let stem = template
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "run".into());
    let ext = template
        .extension()
        .map(|e| e.to_string_lossy().to_string())
        .unwrap_or_else(|| "json".into());
    template.with_file_name(format!("{}.part-{}-of-{}.{}", stem, index, slice.count, ext))
}

fn fnv1a(bytes: &[u8]) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in bytes {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{:016x}", hash)
}

fn host_name() -> Option<String> {
    std::env::var("SLURMD_NODENAME")
        .ok()
        .or_else(|| std::env::var("HOSTNAME").ok())
        .or_else(|| fs::read_to_string("/etc/hostname").ok().map(|s| s.trim().to_string()))
        .filter(|s| !s.is_empty())
}

fn slurm_job_id() -> Option<String> {
    match (std::env::var("SLURM_ARRAY_JOB_ID"), std::env::var("SLURM_ARRAY_TASK_ID")) {
        (Ok(job), Ok(task)) => Some(format!("{}_{}", job, task)),
        _ => std::env::var("SLURM_JOB_ID").ok(),
    }
}

// ---------------------------------------------------------------------------
// Checkpoints
// ---------------------------------------------------------------------------

/// Identifies a sweep for its checkpoint: the design and every setting that
/// changes the simulated points.
fn sweep_key(design_hash: &str, config: &RobustnessConfig) -> Value {
    json!({
        "design": design_hash,
        "x_axis": config.x_axis,
        "y_axis": config.y_axis,
        "expected_behavior": config.expected_behavior,
        "cell_clock_delays": config.cell_clock_delays,
        "thresholds": config.thresholds,
        "slice": config.slice,
    })
}

/// Points of a checkpoint file written for the same sweep; a checkpoint of a
/// different sweep is an error (use --force to discard it).
fn read_checkpoint(path: &Path, key: &Value) -> Result<Vec<SweepPoint>, Box<dyn Error>> {
    let Ok(file) = File::open(path) else {
        return Ok(Vec::new());
    };
    let mut lines = BufReader::new(file).lines();
    let header: Value = match lines.next() {
        Some(Ok(line)) => serde_json::from_str(&line).unwrap_or(Value::Null),
        _ => return Ok(Vec::new()),
    };
    if header["format"] != CHECKPOINT_FORMAT || header["sweep"] != *key {
        return Err(format!(
            "Checkpoint {} belongs to a different sweep; delete it or use --force",
            path.display()
        )
        .into());
    }
    // A task killed while writing leaves at most one incomplete last line.
    Ok(lines
        .map_while(Result::ok)
        .filter_map(|line| serde_json::from_str::<SweepPoint>(&line).ok())
        .collect())
}

struct CliObserver {
    checkpoint: Mutex<File>,
    interval: Duration,
    started: Instant,
    last: Mutex<Instant>,
    label: String,
}

impl SweepObserver for CliObserver {
    fn progress(&self, p: &RobustnessProgress) {
        let mut last = self.last.lock().unwrap();
        let final_report = p.completed == p.total;
        if last.elapsed() < self.interval && !final_report {
            return;
        }
        *last = Instant::now();
        let elapsed = self.started.elapsed().as_secs_f64();
        let eta = if p.fraction > 0.0 && p.fraction < 1.0 {
            format!(", ETA {:.0} s", elapsed * (1.0 - p.fraction) / p.fraction)
        } else {
            String::new()
        };
        eprintln!(
            "[{:8.1} s] {}{}/{} points ({:.1} %){}",
            elapsed,
            self.label,
            p.completed,
            p.total,
            100.0 * p.fraction,
            eta
        );
    }

    fn point(&self, point: &SweepPoint) {
        if let Ok(line) = serde_json::to_string(point) {
            let mut file = self.checkpoint.lock().unwrap();
            let _ = writeln!(file, "{}", line);
            let _ = file.flush();
        }
    }
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

fn run(m: &ArgMatches) -> Result<(), Box<dyn Error>> {
    let design_path = m.get_one::<PathBuf>("design").unwrap();
    let config_path = m.get_one::<PathBuf>("config").unwrap();
    let (raw, nominal, designer_properties, design_hash) = read_design(design_path)?;
    let mut config = read_config(config_path)?;
    config.designer_properties = designer_properties;

    let slice = m.get_one::<SweepSlice>("slice").copied().or_else(|| {
        if m.get_flag("no-slurm") {
            None
        } else {
            slurm_slice()
        }
    });
    config.slice = slice.filter(|s| s.count > 1);
    let threads = m
        .get_one::<usize>("threads")
        .copied()
        .or_else(|| (!m.get_flag("no-slurm")).then(|| env_usize("SLURM_CPUS_PER_TASK")).flatten())
        .or(config.max_threads);
    config.max_threads = threads;
    if let Some(dir) = m.get_one::<PathBuf>("variants-dir") {
        config.output_dir = Some(dir.to_string_lossy().to_string());
    }
    if m.get_flag("no-variants") {
        config.output_dir = None;
    }

    let output = output_path(m.get_one::<PathBuf>("output").unwrap(), config.slice);
    let force = m.get_flag("force");
    if output.exists() && !force {
        let complete = fs::read_to_string(&output)
            .ok()
            .and_then(|text| RobustnessRun::from_json(&text).ok())
            .is_some_and(|run| !run.cancelled);
        if complete {
            eprintln!("{} already exists; nothing to do (use --force to run again)", output.display());
            return Ok(());
        }
    }
    if let Some(parent) = output.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }

    let checkpoint_path = PathBuf::from(format!("{}.checkpoint.jsonl", output.display()));
    let key = sweep_key(&design_hash, &config);
    let completed = if force {
        Vec::new()
    } else {
        read_checkpoint(&checkpoint_path, &key)?
    };
    let mut checkpoint = if completed.is_empty() {
        let mut file = File::create(&checkpoint_path)?;
        writeln!(file, "{}", json!({"format": CHECKPOINT_FORMAT, "sweep": key}))?;
        file
    } else {
        OpenOptions::new().append(true).open(&checkpoint_path)?
    };
    // Terminate an incomplete last line left by a killed task.
    writeln!(checkpoint)?;

    let name = m.get_one::<String>("name").cloned().unwrap_or_else(|| {
        design_path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "sweep".into())
    });
    let label = match config.slice {
        Some(s) => format!("slice {}/{}: ", s.index, s.count),
        None => String::new(),
    };
    let points_in_slice = match config.slice {
        Some(s) => slice_sizes(&config, s.count)[s.index],
        None => config.num_points(),
    };
    eprintln!(
        "qca-sim robustness: {} ({} of {} points{}, {} threads{}){}",
        name,
        points_in_slice,
        config.num_points(),
        config.slice.map(|s| format!(", slice {}/{}", s.index, s.count)).unwrap_or_default(),
        config
            .max_threads
            .map(|t| t.to_string())
            .unwrap_or_else(|| "all".into()),
        host_name().map(|h| format!(" on {}", h)).unwrap_or_default(),
        if completed.is_empty() {
            String::new()
        } else {
            format!(", resuming with {} finished points", completed.len())
        }
    );

    let observer = CliObserver {
        checkpoint: Mutex::new(checkpoint),
        interval: Duration::from_secs(*m.get_one::<u64>("progress-interval").unwrap()),
        started: Instant::now(),
        last: Mutex::new(Instant::now()),
        label,
    };
    let options = SweepOptions {
        app_version: format!("qca-sim {}", env!("CARGO_PKG_VERSION")),
        max_points: *m.get_one::<usize>("max-points").unwrap(),
        completed_points: completed,
    };
    let slice = config.slice;
    let result = run_sweep(
        &observer,
        raw,
        nominal,
        config,
        Arc::new(AtomicBool::new(false)),
        options,
    )?;
    let mut run = RobustnessRun::from_result(&name, result);
    run.slices = vec![RunSliceInfo {
        index: slice.map_or(0, |s| s.index),
        count: slice.map_or(1, |s| s.count),
        points: run.points.len(),
        duration_ms: run.duration_ms,
        host: host_name(),
        job_id: slurm_job_id(),
    }];
    let errors = run.points.iter().filter(|p| p.error.is_some()).count();
    let tmp = output.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_string(&run)?)?;
    fs::rename(&tmp, &output)?;
    let _ = fs::remove_file(&checkpoint_path);
    eprintln!(
        "wrote {}: {} points, {} errors, wall {:.1} s, simulation time {:.1} s",
        output.display(),
        run.points.len(),
        errors,
        run.duration_ms as f64 / 1000.0,
        run.cpu_ms.unwrap_or(0) as f64 / 1000.0
    );
    Ok(())
}

fn collect_run_files(inputs: &[PathBuf]) -> Result<Vec<PathBuf>, Box<dyn Error>> {
    let mut files = Vec::new();
    for input in inputs {
        if input.is_dir() {
            let mut found: Vec<PathBuf> = fs::read_dir(input)?
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|e| e == "json"))
                .collect();
            found.sort();
            files.extend(found);
        } else {
            files.push(input.clone());
        }
    }
    Ok(files)
}

fn merge(m: &ArgMatches) -> Result<(), Box<dyn Error>> {
    let inputs: Vec<PathBuf> = m.get_many::<PathBuf>("inputs").unwrap().cloned().collect();
    let output = m.get_one::<PathBuf>("output").unwrap();
    let mut runs = Vec::new();
    for file in collect_run_files(&inputs)? {
        if fs::canonicalize(&file).ok() == fs::canonicalize(output).ok() {
            continue;
        }
        let text = fs::read_to_string(&file)?;
        match RobustnessRun::from_json(&text) {
            Ok(run) => runs.push(run),
            // Folders may contain other JSON files (e.g. the configuration).
            Err(e) if inputs.iter().any(|i| i.is_dir()) => {
                eprintln!("skipping {}: {}", file.display(), e)
            }
            Err(e) => return Err(format!("{}: {}", file.display(), e).into()),
        }
    }
    let name = m
        .get_one::<String>("name")
        .cloned()
        .or_else(|| runs.first().map(|r| r.name.clone()))
        .unwrap_or_else(|| "merged".into());
    let (merged, report) = merge_runs(&name, runs)?;
    eprintln!(
        "merged {} run files: {} of {} points, {} duplicates, {} missing",
        report.runs,
        report.points,
        merged.grid_size(),
        report.duplicates,
        report.missing
    );
    if report.missing > 0 && !m.get_flag("allow-incomplete") {
        let missing: Vec<String> = merged
            .missing_points()
            .iter()
            .take(10)
            .map(|(ix, iy)| format!("({}, {})", ix, iy))
            .collect();
        return Err(format!(
            "{} grid points are missing, e.g. (ix, iy) = {}; rerun the failed tasks or pass --allow-incomplete",
            report.missing,
            missing.join(", ")
        )
        .into());
    }
    if let Some(parent) = output.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    fs::write(output, serde_json::to_string(&merged)?)?;
    let accuracies: Vec<f64> = merged.points.iter().filter_map(|p| p.accuracy).collect();
    let correct = accuracies.iter().filter(|a| **a >= 1.0 - 1e-9).count();
    eprintln!(
        "wrote {}: {} fully correct, {} with errors; simulation time {:.2} core-hours, longest slice {:.1} s",
        output.display(),
        correct,
        merged.points.iter().filter(|p| p.error.is_some()).count(),
        merged.cpu_ms.unwrap_or(0) as f64 / 3.6e6,
        merged.duration_ms as f64 / 1000.0
    );
    Ok(())
}

fn summary(m: &ArgMatches) -> Result<(), Box<dyn Error>> {
    if m.get_flag("header") {
        println!(
            "name\tpoints\tgrid\tmissing\terrors\tfully_correct\tmean_accuracy\tmin_accuracy\tsimulation_core_hours\twall_s\tslices\tfile"
        );
    }
    for file in m.get_many::<PathBuf>("inputs").unwrap() {
        let run = RobustnessRun::from_json(&fs::read_to_string(file)?)
            .map_err(|e| format!("{}: {}", file.display(), e))?;
        let accuracies: Vec<f64> = run.points.iter().filter_map(|p| p.accuracy).collect();
        let mean = accuracies.iter().sum::<f64>() / accuracies.len().max(1) as f64;
        let min = accuracies.iter().copied().fold(f64::INFINITY, f64::min);
        println!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{:.4}\t{:.4}\t{:.3}\t{:.1}\t{}\t{}",
            run.name,
            run.points.len(),
            run.grid_size(),
            run.missing_points().len(),
            run.points.iter().filter(|p| p.error.is_some()).count(),
            accuracies.iter().filter(|a| **a >= 1.0 - 1e-9).count(),
            mean,
            if min.is_finite() { min } else { f64::NAN },
            run.cpu_ms.unwrap_or_else(|| run.points.iter().map(|p| p.duration_ms).sum()) as f64 / 3.6e6,
            run.duration_ms as f64 / 1000.0,
            run.slices.len().max(1),
            file.display()
        );
    }
    Ok(())
}

fn plan(m: &ArgMatches) -> Result<(), Box<dyn Error>> {
    let (_, nominal, _, _) = read_design(m.get_one::<PathBuf>("design").unwrap())?;
    let config = read_config(m.get_one::<PathBuf>("config").unwrap())?;
    // Everything a run checks before it simulates.
    qca_core::analysis::robustness::validate_config(&nominal, &config)?;
    let n = config.num_points();
    let tasks = m.get_one::<usize>("tasks").copied();
    let sizes = tasks.map(|t| slice_sizes(&config, t));
    if m.get_flag("json") {
        println!(
            "{}",
            json!({
                "points": n,
                "x": config.x_axis,
                "y": config.y_axis,
                "expected_behavior": config.expected_behavior,
                "reference_simulation": config.expected_behavior
                    == qca_core::analysis::robustness::ExpectedBehavior::Reference,
                "tasks": tasks,
                "points_per_task": sizes,
            })
        );
    } else {
        println!(
            "{} points: {} x {}{}",
            n,
            config.x_axis.parameter,
            config.x_axis.values.len(),
            config
                .y_axis
                .as_ref()
                .map(|y| format!(", {} x {}", y.parameter, y.values.len()))
                .unwrap_or_default()
        );
        if let (Some(t), Some(sizes)) = (tasks, sizes) {
            println!(
                "{} tasks: {} to {} points per task",
                t,
                sizes.iter().min().unwrap_or(&0),
                sizes.iter().max().unwrap_or(&0)
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_expand_inclusively() {
        let mut v = Value::from("40:50:2.5");
        expand_range(&mut v).unwrap();
        assert_eq!(v, json!([40.0, 42.5, 45.0, 47.5, 50.0]));
        let mut v = Value::from("0.1:0.3:0.1");
        expand_range(&mut v).unwrap();
        assert_eq!(v, json!([0.1, 0.2, 0.3]));
        assert!(expand_range(&mut Value::from("1:0:1")).is_err());
        assert!(expand_range(&mut Value::from("a:b")).is_err());
    }

    #[test]
    fn slice_output_names() {
        let s = Some(SweepSlice { index: 3, count: 16 });
        assert_eq!(output_path(Path::new("out/run.json"), s), PathBuf::from("out/run.part-03-of-16.json"));
        assert_eq!(output_path(Path::new("r_{index}.json"), s), PathBuf::from("r_03.json"));
        assert_eq!(output_path(Path::new("run.json"), None), PathBuf::from("run.json"));
    }
}
