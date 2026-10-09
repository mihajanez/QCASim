//! Robustness analysis: sweeps one or two design/model parameters, simulates
//! every resulting design variant and scores each variant's truth table
//! against the expected logic behaviour.
//!
//! This is a native port of the QCASim `scripts/` pipeline
//! (`gen_designs.py` -> `run_sim.py` -> `analyze_truth.py`). It is the single
//! implementation behind the QCAForge **Robustness** view and the
//! `qca-sim robustness` command. Generated `.qcd`/`.qcs` files use the same
//! `<name>_<x>_<y>` naming as the scripts, so they stay usable there.
//!
//! Because the variants of a sweep are independent, a sweep can be split into
//! slices ([`SweepSlice`]) that run as separate processes, e.g. the tasks of a
//! Slurm job array, and whose partial run files are combined afterwards with
//! [`merge_runs`].

use crate::analysis::truth_table::generate_truth_table;
use crate::design::file::QCADesign;
use crate::objects::cell::{CellType, QCACellIndex};
use crate::simulation::file::{write_to_file, QCASimulationData};
use crate::simulation::models::{create_sim_model, prepare_simulation};
use crate::simulation::settings::{InputDescriptor, OptionsEntry};
use crate::simulation::{run_simulation_async, SimulationCancelRequest, SimulationProgress};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Default upper bound on the number of points of one sweep (the limit the
/// desktop application applies; headless runs may raise it).
pub const DEFAULT_MAX_SWEEP_POINTS: usize = 10_000;
const PROGRESS_INTERVAL: Duration = Duration::from_millis(200);

/// Identifier of the robustness run file format.
pub const ROBUSTNESS_RUN_FORMAT: &str = "qcaforge-robustness-analysis";
/// Version of the robustness run file format.
pub const ROBUSTNESS_RUN_VERSION: u32 = 1;

// ---------------------------------------------------------------------------
// Sweep parameters
// ---------------------------------------------------------------------------

/// A sweepable parameter, addressed by the same ids the model parameter
/// export and the Python scripts (`model_params.py`) use:
/// `geometry.cell_size`, `geometry.dot_radius`, `geometry.dot_diameter`,
/// `geometry.layer_z:<layer>`, `geometry.cell_offset_x:<label>`,
/// `geometry.cell_offset_y:<label>`, `model.<key>` and `clock.<key>`.
#[derive(Clone, Debug, PartialEq)]
enum SweepTarget {
    CellSize,
    DotRadius,
    DotDiameter,
    LayerZ(usize),
    CellOffset { label: String, axis: usize },
    Model(String),
    Clock(String),
}

impl FromStr for SweepTarget {
    type Err = String;

    fn from_str(id: &str) -> Result<Self, Self::Err> {
        let invalid = || format!("Unknown parameter '{}'", id);
        let (group, name) = id.split_once('.').ok_or_else(invalid)?;
        match group {
            "model" if !name.is_empty() => Ok(SweepTarget::Model(name.to_string())),
            "clock" if !name.is_empty() => Ok(SweepTarget::Clock(name.to_string())),
            "geometry" => {
                let (name, arg) = match name.split_once(':') {
                    Some((name, arg)) => (name, Some(arg)),
                    None => (name, None),
                };
                match (name, arg) {
                    ("cell_size", None) => Ok(SweepTarget::CellSize),
                    ("dot_radius", None) => Ok(SweepTarget::DotRadius),
                    ("dot_diameter", None) => Ok(SweepTarget::DotDiameter),
                    ("layer_z", Some(layer)) => layer
                        .parse()
                        .map(SweepTarget::LayerZ)
                        .map_err(|_| invalid()),
                    ("cell_offset_x", Some(label)) if !label.is_empty() => {
                        Ok(SweepTarget::CellOffset {
                            label: label.to_string(),
                            axis: 0,
                        })
                    }
                    ("cell_offset_y", Some(label)) if !label.is_empty() => {
                        Ok(SweepTarget::CellOffset {
                            label: label.to_string(),
                            axis: 1,
                        })
                    }
                    _ => Err(invalid()),
                }
            }
            _ => Err(invalid()),
        }
    }
}

fn used_architecture_ids(design: &QCADesign) -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    for layer in &design.layers {
        if !ids.contains(&layer.cell_architecture_id) {
            ids.push(layer.cell_architecture_id.clone());
        }
    }
    ids
}

fn selected_model_id(design: &QCADesign) -> Result<String, String> {
    design
        .simulation_settings
        .selected_simulation_model_id
        .clone()
        .ok_or_else(|| "No simulation model is selected".to_string())
}

fn settings_object<'a>(
    design: &'a mut QCADesign,
    clock: bool,
) -> Result<&'a mut serde_json::Map<String, Value>, String> {
    let model_id = selected_model_id(design)?;
    let settings = design
        .simulation_settings
        .simulation_model_settings
        .get_mut(&model_id)
        .ok_or_else(|| format!("Design has no settings for model '{}'", model_id))?;
    let value = if clock {
        &mut settings.clock_generator_settings
    } else {
        &mut settings.model_settings
    };
    value
        .as_object_mut()
        .ok_or_else(|| "Model settings are not an object".to_string())
}

/// Whether a model/clock option only accepts whole numbers (serde would
/// reject e.g. `1.0` for a `usize` field).
fn is_whole_number_option(model_id: &str, key: &str, clock: bool) -> bool {
    let Some(model) = create_sim_model(model_id) else {
        return false;
    };
    let options = if clock {
        model.get_clock_generator_options_list()
    } else {
        model.get_model_options_list()
    };
    options.iter().any(|option| match option {
        OptionsEntry::Input {
            unique_id,
            descriptor: InputDescriptor::NumberInput { whole_num, .. },
            ..
        } => unique_id == key && *whole_num,
        _ => false,
    })
}

fn get_parameter(design: &QCADesign, target: &SweepTarget) -> Result<f64, String> {
    let first_architecture = || {
        design
            .layers
            .first()
            .and_then(|layer| design.cell_architectures.get(&layer.cell_architecture_id))
            .ok_or_else(|| "Design has no layers or cell architectures".to_string())
    };
    match target {
        SweepTarget::CellSize => Ok(first_architecture()?.side_length),
        SweepTarget::DotRadius => {
            let [x, y] = *first_architecture()?
                .dot_positions
                .first()
                .ok_or("Cell architecture has no dots")?;
            Ok(x.hypot(y))
        }
        SweepTarget::DotDiameter => Ok(first_architecture()?.dot_diameter),
        SweepTarget::LayerZ(layer) => design
            .layers
            .get(*layer)
            .map(|layer| layer.z_position)
            .ok_or_else(|| format!("Layer {} does not exist", layer)),
        SweepTarget::CellOffset { .. } => Ok(0.0),
        SweepTarget::Model(key) | SweepTarget::Clock(key) => {
            let model_id = selected_model_id(design)?;
            let settings = design
                .simulation_settings
                .simulation_model_settings
                .get(&model_id)
                .ok_or_else(|| format!("Design has no settings for model '{}'", model_id))?;
            let values = if matches!(target, SweepTarget::Clock(_)) {
                &settings.clock_generator_settings
            } else {
                &settings.model_settings
            };
            values
                .get(key)
                .and_then(Value::as_f64)
                .ok_or_else(|| format!("Model has no numeric parameter '{}'", key))
        }
    }
}

fn set_parameter(design: &mut QCADesign, target: &SweepTarget, value: f64) -> Result<(), String> {
    match target {
        SweepTarget::CellSize => {
            let nominal = get_parameter(design, target)?;
            if nominal <= 0.0 {
                return Err("Cell size of the design must be positive".into());
            }
            let factor = value / nominal;
            for layer in &mut design.layers {
                for cell in &mut layer.cells {
                    cell.position = cell.position.map(|p| p * factor);
                }
            }
            for id in used_architecture_ids(design) {
                if let Some(architecture) = design.cell_architectures.get_mut(&id) {
                    architecture.side_length *= factor;
                }
            }
        }
        SweepTarget::DotRadius => {
            for id in used_architecture_ids(design) {
                if let Some(architecture) = design.cell_architectures.get_mut(&id) {
                    for position in &mut architecture.dot_positions {
                        let radius = position[0].hypot(position[1]);
                        if radius > 0.0 {
                            *position = position.map(|p| p * value / radius);
                        }
                    }
                }
            }
        }
        SweepTarget::DotDiameter => {
            for id in used_architecture_ids(design) {
                if let Some(architecture) = design.cell_architectures.get_mut(&id) {
                    architecture.dot_diameter = value;
                }
            }
        }
        SweepTarget::LayerZ(layer) => {
            design
                .layers
                .get_mut(*layer)
                .ok_or_else(|| format!("Layer {} does not exist", layer))?
                .z_position = value;
        }
        SweepTarget::CellOffset { label, axis } => {
            let mut found = false;
            for layer in &mut design.layers {
                for cell in &mut layer.cells {
                    if cell.label.as_deref() == Some(label.as_str()) {
                        cell.position[*axis] += value;
                        found = true;
                    }
                }
            }
            if !found {
                return Err(format!("No cell is labelled '{}'", label));
            }
        }
        SweepTarget::Model(key) | SweepTarget::Clock(key) => {
            let clock = matches!(target, SweepTarget::Clock(_));
            let model_id = selected_model_id(design)?;
            let whole_num = is_whole_number_option(&model_id, key, clock);
            let settings = settings_object(design, clock)?;
            if !settings.contains_key(key) {
                return Err(format!("Model has no parameter '{}'", key));
            }
            let json_value = if whole_num {
                Value::from(value.round().max(0.0) as u64)
            } else {
                Value::from(value)
            };
            settings.insert(key.clone(), json_value);
        }
    }
    Ok(())
}

fn clone_design(design: &QCADesign) -> Result<QCADesign, String> {
    serde_json::to_value(design)
        .and_then(serde_json::from_value)
        .map_err(|e| format!("Could not copy design: {}", e))
}

/// Builds one design variant from the nominal design. Cell size is applied
/// first so that cell offsets are relative to the rescaled layout (the same
/// order `gen_designs.py` uses for its displacement sweep).
fn create_variant(
    nominal: &QCADesign,
    assignments: &[(&SweepTarget, f64)],
) -> Result<QCADesign, String> {
    let mut design = clone_design(nominal)?;
    let mut ordered: Vec<&(&SweepTarget, f64)> = assignments.iter().collect();
    ordered.sort_by_key(|(target, _)| **target != SweepTarget::CellSize);
    for (target, value) in ordered {
        set_parameter(&mut design, target, *value)?;
    }
    Ok(design)
}

// ---------------------------------------------------------------------------
// Truth tables and expected behaviour
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct TruthTableData {
    /// Row-major logic values, one column per stored (input/output) cell.
    pub rows: Vec<Vec<Option<char>>>,
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ExpectedBehavior {
    /// Compare output cells against the nominal (unmodified) design.
    Reference,
    Wire,
    Inverter,
    Majority,
    MemoryCell,
    Flipflop1,
    /// Ternary toggle flip-flop with synchronous reset (columns T, R, Q); scored sequentially:
    /// R = -1 (A) resets Q to -1, otherwise (R = +1) T = -1 holds, T = +1 toggles and T = 0 clears Q to 0.
    TernaryFlipflop,
}

/// `analyze_truth.py`'s `_equivariance`: states C and D are treated alike.
fn equivalent(value: char) -> char {
    if value == 'D' {
        'C'
    } else {
        value
    }
}

fn same_state(a: Option<char>, b: Option<char>) -> bool {
    matches!((a, b), (Some(a), Some(b)) if equivalent(a) == equivalent(b))
}

fn expected_majority(x: char, y: char, z: char) -> char {
    // Tri-state majority from analyze_truth.py: any two equal inputs win,
    // three distinct inputs give C.
    if x == y || x == z {
        x
    } else if y == z {
        y
    } else {
        'C'
    }
}

fn expected_flipflop1(g1: char, flip: char) -> char {
    match (g1, flip) {
        ('B', 'A') => 'B',
        ('C', 'B') | ('C', 'C') => 'C',
        _ => 'A',
    }
}

impl ExpectedBehavior {
    /// Checks that the stored cell count matches what the comparison expects.
    fn validate_columns(&self, columns: usize) -> Result<(), String> {
        let ok = match self {
            ExpectedBehavior::Reference => columns >= 1,
            ExpectedBehavior::Wire | ExpectedBehavior::Inverter => columns >= 2,
            ExpectedBehavior::Majority => columns == 4,
            ExpectedBehavior::MemoryCell
            | ExpectedBehavior::Flipflop1
            | ExpectedBehavior::TernaryFlipflop => columns == 3,
        };
        if ok {
            return Ok(());
        }
        let expected = match self {
            ExpectedBehavior::Reference => "at least 1 input/output cell",
            ExpectedBehavior::Wire | ExpectedBehavior::Inverter => {
                "at least 2 input/output cells (input first)"
            }
            ExpectedBehavior::Majority => "exactly 4 input/output cells (3 inputs, 1 output)",
            ExpectedBehavior::MemoryCell => "exactly 3 input/output cells (X, W, Q)",
            ExpectedBehavior::Flipflop1 => "exactly 3 input/output cells (Flip, G1, Q)",
            ExpectedBehavior::TernaryFlipflop => "exactly 3 input/output cells (T, R, Q)",
        };
        Err(format!(
            "The selected expected behaviour needs {}, but the design has {}",
            expected, columns
        ))
    }

    /// Accuracy of a single truth table row, as in `analyze_truth.py`.
    fn row_accuracy(&self, row: &[Option<char>]) -> f64 {
        let score = |ok: bool| if ok { 1.0 } else { 0.0 };
        match self {
            ExpectedBehavior::Reference => unreachable!("reference rows are scored separately"),
            ExpectedBehavior::Wire => {
                if row[0].is_none() {
                    return 1.0;
                }
                let outputs = &row[1..];
                outputs.iter().filter(|v| same_state(row[0], **v)).count() as f64
                    / outputs.len() as f64
            }
            ExpectedBehavior::Inverter => {
                let expected = match row[0].map(equivalent) {
                    Some('A') => 'B',
                    Some('B') => 'A',
                    Some('C') => 'C',
                    _ => return 0.0,
                };
                score(same_state(Some(expected), row[row.len() - 1]))
            }
            ExpectedBehavior::Majority => match row {
                [Some(x), Some(y), Some(z), Some(r)] => score(
                    expected_majority(equivalent(*x), equivalent(*y), equivalent(*z))
                        == equivalent(*r),
                ),
                _ => 0.0,
            },
            ExpectedBehavior::MemoryCell => match row {
                // The memory cell's output follows W; undefined inputs are ignored.
                [Some(_), Some(w), q] => score(same_state(Some(*w), *q)),
                _ => 1.0,
            },
            ExpectedBehavior::Flipflop1 => match row {
                [Some(flip), Some(g1), q] => score(same_state(
                    Some(expected_flipflop1(equivalent(*g1), equivalent(*flip))),
                    *q,
                )),
                _ => 1.0,
            },
            ExpectedBehavior::TernaryFlipflop => unreachable!("scored sequentially in score_table"),
        }
    }
}

/// Next state of the ternary toggle flip-flop with reset for inputs (T, R) and previous state `q`
/// (`None` when the previous state is undefined).
fn expected_ternary_flipflop(t: char, r: char, q: Option<char>) -> Option<char> {
    if r == 'A' {
        return Some('A');
    }
    match t {
        'A' => q.map(equivalent),
        'B' => q.map(|q| match equivalent(q) {
            'A' => 'B',
            'B' => 'A',
            other => other,
        }),
        _ => Some('C'),
    }
}

/// A row is correct when its Q equals the next state computed from the row's inputs and the
/// previous row's (simulated) Q. A row whose expected state depends on an undefined previous
/// state is counted as incorrect; rows with undefined inputs are ignored.
fn ternary_flipflop_accuracy(row: &[Option<char>], previous: Option<char>) -> f64 {
    match row {
        [Some(t), Some(r), q] => {
            match expected_ternary_flipflop(equivalent(*t), equivalent(*r), previous) {
                Some(expected) => {
                    if same_state(Some(expected), *q) {
                        1.0
                    } else {
                        0.0
                    }
                }
                None => 0.0,
            }
        }
        _ => 1.0,
    }
}

/// Returns the table accuracy (mean row accuracy) and every row's accuracy.
fn score_table(
    behavior: ExpectedBehavior,
    table: &TruthTableData,
    reference: Option<&TruthTableData>,
    output_columns: &[bool],
) -> (f64, Vec<f64>) {
    let row_scores: Vec<f64> = match (behavior, reference) {
        (ExpectedBehavior::TernaryFlipflop, _) => table
            .rows
            .iter()
            .enumerate()
            .map(|(i, row)| {
                let previous = if i == 0 {
                    None
                } else {
                    table.rows[i - 1].get(2).copied().flatten()
                };
                ternary_flipflop_accuracy(row, previous)
            })
            .collect(),
        (ExpectedBehavior::Reference, Some(reference)) => reference
            .rows
            .iter()
            .enumerate()
            .map(|(i, reference_row)| {
                let row = table.rows.get(i);
                let (matched, counted) = reference_row
                    .iter()
                    .enumerate()
                    .filter(|(j, value)| output_columns.get(*j) == Some(&true) && value.is_some())
                    .fold((0usize, 0usize), |(matched, counted), (j, value)| {
                        let actual = row.and_then(|row| row.get(j).copied().flatten());
                        (matched + same_state(*value, actual) as usize, counted + 1)
                    });
                if counted == 0 {
                    1.0
                } else {
                    matched as f64 / counted as f64
                }
            })
            .collect(),
        _ => table
            .rows
            .iter()
            .map(|row| behavior.row_accuracy(row))
            .collect(),
    };
    let accuracy = if row_scores.is_empty() {
        1.0
    } else {
        row_scores.iter().sum::<f64>() / row_scores.len() as f64
    };
    (accuracy, row_scores)
}

#[cfg(test)]
fn table_accuracy(
    behavior: ExpectedBehavior,
    table: &TruthTableData,
    reference: Option<&TruthTableData>,
    output_columns: &[bool],
) -> f64 {
    score_table(behavior, table, reference, output_columns).0
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, PartialEq)]
pub struct TruthTableThresholds {
    pub clock: f64,
    pub logical: f64,
    pub value: f64,
}

fn compute_truth_table(
    design: &QCADesign,
    simulation: &QCASimulationData,
    delays: &HashMap<QCACellIndex, usize>,
    thresholds: TruthTableThresholds,
) -> TruthTableData {
    let table = generate_truth_table(
        design,
        simulation,
        &simulation.metadata.stored_cells,
        delays.clone(),
        thresholds.clock,
        thresholds.logical,
        thresholds.value,
    );
    let num_rows = table
        .entries
        .iter()
        .map(|(_, v)| v.len())
        .max()
        .unwrap_or(0);
    let rows = (0..num_rows)
        .map(|i| {
            table
                .entries
                .iter()
                .map(|(_, values)| values.get(i).copied().flatten())
                .collect()
        })
        .collect();
    TruthTableData { rows }
}

/// Input/output cells in the order the simulation stores them (and so the
/// order of the truth table columns).
fn stored_cells(design: &QCADesign) -> Vec<(QCACellIndex, String, bool)> {
    let mut cells = Vec::new();
    for (l, layer) in design.layers.iter().enumerate() {
        for (c, cell) in layer.cells.iter().enumerate() {
            if matches!(cell.typ, CellType::Input | CellType::Output) {
                let index = QCACellIndex::new(l, c);
                let label = cell.label.clone().unwrap_or_else(|| index.to_string());
                cells.push((index, label, cell.typ == CellType::Output));
            }
        }
    }
    cells
}

/// Resolves `<CellIndex|CellLabel>` keys, like `qca-sim truth -d`.
fn resolve_cell_delays(
    design: &QCADesign,
    delays: &HashMap<String, usize>,
) -> Result<HashMap<QCACellIndex, usize>, String> {
    delays
        .iter()
        .filter(|(_, delay)| **delay > 0)
        .map(|(key, delay)| {
            let index = QCACellIndex::from_str(key).or_else(|_| {
                stored_cells(design)
                    .into_iter()
                    .find(|(_, label, _)| label == key)
                    .map(|(index, _, _)| index)
                    .ok_or_else(|| format!("Could not find cell with label '{}'", key))
            })?;
            Ok((index, *delay))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Running the sweep
// ---------------------------------------------------------------------------

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
pub struct SweepAxis {
    pub parameter: String,
    pub values: Vec<f64>,
}

/// Selects every `count`-th point of a sweep, starting at point `index`
/// (0-based), in the row-major order of the sweep grid. Interleaving the
/// points spreads slowly converging variants, which cluster near operating
/// boundaries, evenly over the slices.
#[derive(Deserialize, Serialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct SweepSlice {
    pub index: usize,
    pub count: usize,
}

impl SweepSlice {
    pub fn validate(&self) -> Result<(), String> {
        if self.count == 0 || self.index >= self.count {
            return Err(format!(
                "Invalid slice {}/{}: the index must be in 0..{}",
                self.index, self.count, self.count
            ));
        }
        Ok(())
    }

    /// Whether the point with the given row-major grid index belongs to this slice.
    pub fn contains(&self, point_index: usize) -> bool {
        point_index % self.count == self.index
    }
}

impl FromStr for SweepSlice {
    type Err = String;

    /// Parses `<index>/<count>`, e.g. `3/16`.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (index, count) = s
            .split_once('/')
            .ok_or_else(|| format!("Slice '{}' must have the form <index>/<count>", s))?;
        let slice = SweepSlice {
            index: index
                .trim()
                .parse()
                .map_err(|_| format!("Invalid slice index '{}'", index))?,
            count: count
                .trim()
                .parse()
                .map_err(|_| format!("Invalid slice count '{}'", count))?,
        };
        slice.validate()?;
        Ok(slice)
    }
}

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct RobustnessConfig {
    pub x_axis: SweepAxis,
    pub y_axis: Option<SweepAxis>,
    pub expected_behavior: ExpectedBehavior,
    #[serde(default)]
    pub cell_clock_delays: HashMap<String, usize>,
    pub thresholds: TruthTableThresholds,
    /// When set, every variant's `.qcd` and `.qcs` file is kept here.
    #[serde(default)]
    pub output_dir: Option<String>,
    #[serde(default = "default_base_name")]
    pub base_name: String,
    #[serde(default)]
    pub designer_properties: Value,
    #[serde(default)]
    pub max_threads: Option<usize>,
    /// Only simulate this slice of the sweep (see [`SweepSlice`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slice: Option<SweepSlice>,
}

impl RobustnessConfig {
    /// Number of points of the whole sweep grid.
    pub fn num_points(&self) -> usize {
        self.x_axis.values.len() * self.y_axis.as_ref().map_or(1, |axis| axis.values.len())
    }

    /// Whether two configurations describe the same sweep (they may differ
    /// in the slice, the thread count and where variant files are kept).
    pub fn same_sweep(&self, other: &RobustnessConfig) -> bool {
        self.x_axis == other.x_axis
            && self.y_axis == other.y_axis
            && self.expected_behavior == other.expected_behavior
            && self.cell_clock_delays == other.cell_clock_delays
            && self.thresholds == other.thresholds
    }
}

fn default_base_name() -> String {
    "design".into()
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SweepPoint {
    pub ix: usize,
    pub iy: usize,
    pub x: f64,
    pub y: Option<f64>,
    pub accuracy: Option<f64>,
    pub error: Option<String>,
    pub truth_table: Option<TruthTableData>,
    /// Accuracy of each truth table row (reference rows in reference mode).
    #[serde(default)]
    pub row_accuracy: Vec<f64>,
    #[serde(default)]
    pub design_file: Option<String>,
    #[serde(default)]
    pub simulation_file: Option<String>,
    #[serde(default)]
    pub duration_ms: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct RobustnessColumn {
    pub label: String,
    pub is_output: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct RobustnessResult {
    pub config: RobustnessConfig,
    pub model_id: String,
    pub nominal_x: f64,
    pub nominal_y: Option<f64>,
    pub columns: Vec<RobustnessColumn>,
    pub reference_truth_table: Option<TruthTableData>,
    pub points: Vec<SweepPoint>,
    pub cancelled: bool,
    pub duration_ms: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct RobustnessProgress {
    pub completed: usize,
    pub total: usize,
    /// Overall progress in [0, 1], including partially simulated points.
    pub fraction: f64,
}

/// Receives progress reports and finished points while a sweep runs. The
/// desktop application forwards them to the GUI as events; the CLI prints
/// progress and appends points to a checkpoint file.
pub trait SweepObserver: Sync {
    fn progress(&self, _progress: &RobustnessProgress) {}
    fn point(&self, _point: &SweepPoint) {}
}

/// An observer that ignores every report.
pub struct NoObserver;
impl SweepObserver for NoObserver {}

/// Options of a sweep that are not part of its (recorded) configuration.
#[derive(Clone, Debug)]
pub struct SweepOptions {
    /// Written as `qca_forge_version` into the `.qcd` files of the variants.
    pub app_version: String,
    /// Refuse sweeps with more points than this.
    pub max_points: usize,
    /// Points finished by an earlier, interrupted run of the same sweep
    /// (e.g. read from a checkpoint file); they are kept and not simulated
    /// again.
    pub completed_points: Vec<SweepPoint>,
}

impl Default for SweepOptions {
    fn default() -> Self {
        Self {
            app_version: crate::get_qca_core_version(),
            max_points: DEFAULT_MAX_SWEEP_POINTS,
            completed_points: Vec::new(),
        }
    }
}

/// Runs one simulation; returns `Ok(None)` if it was cancelled.
fn simulate(
    design: &QCADesign,
    cancel: &AtomicBool,
    mut on_progress: impl FnMut(f64),
) -> Result<Option<QCASimulationData>, String> {
    let (model, custom_input_sequence) = prepare_simulation(design)?;
    let (handle, progress_rx, cancel_tx) = run_simulation_async(
        model,
        design.layers.clone(),
        design.cell_architectures.clone(),
        custom_input_sequence,
    );
    let mut cancel_tx = Some(cancel_tx);
    loop {
        match progress_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(SimulationProgress::Running {
                current_sample,
                total_samples,
            }) => on_progress(current_sample as f64 / total_samples.max(1) as f64),
            Ok(_) | Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        if cancel.load(Ordering::Relaxed) {
            if let Some(tx) = cancel_tx.take() {
                let _ = tx.send(SimulationCancelRequest {});
            }
        }
    }
    let data = handle
        .join()
        .map_err(|_| "Simulation thread panicked".to_string())?;
    Ok(if cancel.load(Ordering::Relaxed) {
        None
    } else {
        Some(data)
    })
}

fn format_value(value: f64) -> String {
    // Rust's Display never uses exponent notation, and trims trailing zeros.
    let rounded = (value * 1e9).round() / 1e9;
    format!("{}", rounded)
}

/// Adds keys that exist in `source` but not in `target` (recursively, and
/// element-wise for arrays of equal length).
fn merge_missing(target: &mut Value, source: &Value) {
    match (target, source) {
        (Value::Object(target), Value::Object(source)) => {
            for (key, source_value) in source {
                match target.get_mut(key) {
                    Some(target_value) => merge_missing(target_value, source_value),
                    None => {
                        target.insert(key.clone(), source_value.clone());
                    }
                }
            }
        }
        (Value::Array(target), Value::Array(source)) if target.len() == source.len() => {
            for (target_value, source_value) in target.iter_mut().zip(source) {
                merge_missing(target_value, source_value);
            }
        }
        _ => {}
    }
}

fn write_variant_files(
    config: &RobustnessConfig,
    app_version: &str,
    nominal_raw: &Value,
    design: &QCADesign,
    simulation: &QCASimulationData,
    x: f64,
    y: Option<f64>,
) -> Result<(String, String), String> {
    let dir = PathBuf::from(config.output_dir.as_ref().unwrap());
    // Always two numeric suffixes, which is what the Python scripts parse.
    let stem = format!(
        "{}_{}_{}",
        config.base_name,
        format_value(x),
        format_value(y.unwrap_or(0.0))
    );
    let design_path = dir.join(format!("{}.qcd", stem));
    let simulation_path = dir.join(format!("{}.qcs", stem));

    let mut design_value = serde_json::to_value(design).map_err(|e| e.to_string())?;
    // Restore GUI-only fields the qca-core structs drop (layer visibility,
    // architecture names, ...); without `visible` the designer would open
    // the variant with every layer hidden.
    merge_missing(&mut design_value, nominal_raw);
    let design_file = serde_json::json!({
        "qca_forge_version": app_version,
        "design": design_value,
        "designer_properties": config.designer_properties,
    });
    std::fs::write(
        &design_path,
        serde_json::to_string_pretty(&design_file).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("Could not write {}: {}", design_path.display(), e))?;

    let file = File::create(&simulation_path)
        .map_err(|e| format!("Could not create {}: {}", simulation_path.display(), e))?;
    write_to_file(file, design, simulation)
        .map_err(|e| format!("Could not write {}: {}", simulation_path.display(), e))?;

    Ok((
        design_path.to_string_lossy().to_string(),
        simulation_path.to_string_lossy().to_string(),
    ))
}

struct SweepJob {
    ix: usize,
    iy: usize,
    x: f64,
    y: Option<f64>,
}

struct SweepContext {
    config: RobustnessConfig,
    /// The design exactly as the GUI sent it, including GUI-only fields.
    nominal_raw: Value,
    nominal: QCADesign,
    x_target: SweepTarget,
    y_target: Option<SweepTarget>,
    delays: HashMap<QCACellIndex, usize>,
    reference: Option<TruthTableData>,
    output_columns: Vec<bool>,
    app_version: String,
}

fn run_job(
    context: &SweepContext,
    job: &SweepJob,
    cancel: &AtomicBool,
    on_progress: impl FnMut(f64),
) -> Option<SweepPoint> {
    let start = Instant::now();
    let mut point = SweepPoint {
        ix: job.ix,
        iy: job.iy,
        x: job.x,
        y: job.y,
        accuracy: None,
        error: None,
        truth_table: None,
        row_accuracy: Vec::new(),
        design_file: None,
        simulation_file: None,
        duration_ms: 0,
    };

    let mut assignments = vec![(&context.x_target, job.x)];
    if let (Some(target), Some(y)) = (&context.y_target, job.y) {
        assignments.push((target, y));
    }

    let result = (|| -> Result<Option<()>, String> {
        let design = create_variant(&context.nominal, &assignments)?;
        let Some(simulation) = simulate(&design, cancel, on_progress)? else {
            return Ok(None);
        };
        let table = compute_truth_table(
            &design,
            &simulation,
            &context.delays,
            context.config.thresholds,
        );
        let (accuracy, row_accuracy) = score_table(
            context.config.expected_behavior,
            &table,
            context.reference.as_ref(),
            &context.output_columns,
        );
        point.accuracy = Some(accuracy);
        point.row_accuracy = row_accuracy;
        point.truth_table = Some(table);
        if context.config.output_dir.is_some() {
            let (design_file, simulation_file) = write_variant_files(
                &context.config,
                &context.app_version,
                &context.nominal_raw,
                &design,
                &simulation,
                job.x,
                job.y,
            )?;
            point.design_file = Some(design_file);
            point.simulation_file = Some(simulation_file);
        }
        Ok(Some(()))
    })();

    match result {
        Ok(None) => return None,
        Ok(Some(())) => {}
        Err(error) => point.error = Some(error),
    }
    point.duration_ms = start.elapsed().as_millis() as u64;
    Some(point)
}

fn validate_axis(nominal: &QCADesign, axis: &SweepAxis) -> Result<(SweepTarget, f64), String> {
    let target = SweepTarget::from_str(&axis.parameter)?;
    if axis.values.is_empty() {
        return Err(format!(
            "No values given for parameter '{}'",
            axis.parameter
        ));
    }
    if axis.values.iter().any(|v| !v.is_finite()) {
        return Err(format!("Parameter '{}' has invalid values", axis.parameter));
    }
    let nominal_value = get_parameter(nominal, &target)?;
    // Applying the nominal value surfaces e.g. unknown labels before any
    // simulation is started.
    create_variant(nominal, &[(&target, nominal_value)])?;
    Ok((target, nominal_value))
}

struct CheckedSweep {
    model_id: String,
    x_target: SweepTarget,
    nominal_x: f64,
    y_target: Option<SweepTarget>,
    nominal_y: Option<f64>,
    columns: Vec<RobustnessColumn>,
    output_columns: Vec<bool>,
    delays: HashMap<QCACellIndex, usize>,
}

/// Everything a sweep checks before it simulates anything.
fn check_sweep(nominal: &QCADesign, config: &RobustnessConfig) -> Result<CheckedSweep, String> {
    let model_id = selected_model_id(nominal)?;
    prepare_simulation(nominal)?;

    let (x_target, nominal_x) = validate_axis(nominal, &config.x_axis)?;
    let (y_target, nominal_y) = match &config.y_axis {
        Some(axis) => {
            let (target, value) = validate_axis(nominal, axis)?;
            if target == x_target {
                return Err("The two sweep axes must use different parameters".into());
            }
            (Some(target), Some(value))
        }
        None => (None, None),
    };
    if let Some(slice) = &config.slice {
        slice.validate()?;
    }

    let cells = stored_cells(nominal);
    config.expected_behavior.validate_columns(cells.len())?;
    let columns: Vec<RobustnessColumn> = cells
        .iter()
        .map(|(_, label, is_output)| RobustnessColumn {
            label: label.clone(),
            is_output: *is_output,
        })
        .collect();
    let output_columns: Vec<bool> = cells.iter().map(|(_, _, out)| *out).collect();
    if config.expected_behavior == ExpectedBehavior::Reference && !output_columns.contains(&true) {
        return Err("Comparing against the nominal design needs at least one output cell".into());
    }
    let delays = resolve_cell_delays(nominal, &config.cell_clock_delays)?;
    Ok(CheckedSweep {
        model_id,
        x_target,
        nominal_x,
        y_target,
        nominal_y,
        columns,
        output_columns,
        delays,
    })
}

/// Checks a design and sweep configuration (model, parameters, expected
/// behaviour, cell delays) without simulating anything.
pub fn validate_config(nominal: &QCADesign, config: &RobustnessConfig) -> Result<(), String> {
    check_sweep(nominal, config).map(|_| ())
}

/// Runs a robustness sweep: builds every variant of `nominal` on the sweep
/// grid (or on the configured slice of it), simulates the variants on a pool
/// of worker threads and scores their truth tables.
///
/// `nominal_raw` is the design as stored in its `.qcd` file, including fields
/// only the GUI uses; it is needed to write complete variant files.
pub fn run_sweep(
    observer: &dyn SweepObserver,
    nominal_raw: Value,
    nominal: QCADesign,
    config: RobustnessConfig,
    cancel: Arc<AtomicBool>,
    options: SweepOptions,
) -> Result<RobustnessResult, String> {
    let started = Instant::now();
    let CheckedSweep {
        model_id,
        x_target,
        nominal_x,
        y_target,
        nominal_y,
        columns,
        output_columns,
        delays,
    } = check_sweep(&nominal, &config)?;

    let num_points = config.num_points();
    if num_points > options.max_points {
        return Err(format!(
            "The sweep has {} points; the maximum is {}",
            num_points, options.max_points
        ));
    }
    let y_values: Vec<Option<f64>> = match &config.y_axis {
        Some(axis) => axis.values.iter().copied().map(Some).collect(),
        None => vec![None],
    };
    let nx = config.x_axis.values.len();
    let all_jobs = y_values.iter().enumerate().flat_map(|(iy, y)| {
        config
            .x_axis
            .values
            .iter()
            .enumerate()
            .map(move |(ix, x)| SweepJob {
                ix,
                iy,
                x: *x,
                y: *y,
            })
    });
    let in_slice = |job: &SweepJob| {
        config
            .slice
            .map_or(true, |slice| slice.contains(job.iy * nx + job.ix))
    };
    // Points of an earlier run of this slice are kept as they are.
    let mut points: Vec<SweepPoint> = Vec::new();
    let mut done: HashSet<(usize, usize)> = HashSet::new();
    for point in options.completed_points {
        let valid = point.ix < nx
            && point.iy < y_values.len()
            && in_slice(&SweepJob {
                ix: point.ix,
                iy: point.iy,
                x: point.x,
                y: point.y,
            });
        if valid && done.insert((point.ix, point.iy)) {
            points.push(point);
        }
    }
    let resumed = points.len();
    let jobs: Vec<SweepJob> = all_jobs
        .filter(|job| in_slice(job) && !done.contains(&(job.ix, job.iy)))
        .collect();
    if let Some(dir) = &config.output_dir {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("Could not create output folder: {}", e))?;
    }

    let reference_needed = config.expected_behavior == ExpectedBehavior::Reference;
    let total = resumed + jobs.len() + reference_needed as usize;
    let emit_progress = |completed: usize, fraction: f64| {
        observer.progress(&RobustnessProgress {
            completed,
            total,
            fraction,
        });
    };
    emit_progress(resumed, resumed as f64 / total.max(1) as f64);

    let mut cancelled = false;
    let reference = if reference_needed {
        let mut last_emit = Instant::now();
        let simulation = simulate(&nominal, &cancel, |fraction| {
            if last_emit.elapsed() >= PROGRESS_INTERVAL {
                last_emit = Instant::now();
                emit_progress(resumed, (resumed as f64 + fraction) / total as f64);
            }
        })?;
        match simulation {
            Some(simulation) => Some(compute_truth_table(
                &nominal,
                &simulation,
                &delays,
                config.thresholds,
            )),
            None => {
                cancelled = true;
                None
            }
        }
    } else {
        None
    };

    if !cancelled && !jobs.is_empty() {
        let num_threads = config
            .max_threads
            .filter(|n| *n > 0)
            .unwrap_or_else(|| {
                std::thread::available_parallelism()
                    .map(|n| n.get())
                    .unwrap_or(1)
            })
            .min(jobs.len())
            .max(1);

        let context = Arc::new(SweepContext {
            config: config.clone(),
            nominal_raw,
            nominal: clone_design(&nominal)?,
            x_target: x_target.clone(),
            y_target: y_target.clone(),
            delays,
            reference: reference.clone(),
            output_columns,
            app_version: options.app_version.clone(),
        });
        let jobs = Arc::new(jobs);
        let next_job = Arc::new(AtomicUsize::new(0));
        let worker_progress = Arc::new(Mutex::new(vec![0.0f64; num_threads]));
        let (tx, rx) = mpsc::channel::<SweepPoint>();

        let workers: Vec<_> = (0..num_threads)
            .map(|worker| {
                let (context, jobs, next_job, worker_progress, cancel, tx) = (
                    context.clone(),
                    jobs.clone(),
                    next_job.clone(),
                    worker_progress.clone(),
                    cancel.clone(),
                    tx.clone(),
                );
                std::thread::spawn(move || loop {
                    if cancel.load(Ordering::Relaxed) {
                        break;
                    }
                    let index = next_job.fetch_add(1, Ordering::Relaxed);
                    let Some(job) = jobs.get(index) else { break };
                    let point = run_job(&context, job, &cancel, |fraction| {
                        worker_progress.lock().unwrap()[worker] = fraction;
                    });
                    worker_progress.lock().unwrap()[worker] = 0.0;
                    match point {
                        Some(point) => {
                            if tx.send(point).is_err() {
                                break;
                            }
                        }
                        None => break,
                    }
                })
            })
            .collect();
        drop(tx);

        let done_before = resumed + reference_needed as usize;
        let mut finished = 0usize;
        let mut last_emit = Instant::now() - PROGRESS_INTERVAL;
        loop {
            match rx.recv_timeout(PROGRESS_INTERVAL) {
                Ok(point) => {
                    observer.point(&point);
                    points.push(point);
                    finished += 1;
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            if last_emit.elapsed() >= PROGRESS_INTERVAL {
                last_emit = Instant::now();
                let partial: f64 = worker_progress.lock().unwrap().iter().sum();
                let completed = done_before + finished;
                emit_progress(completed, (completed as f64 + partial) / total as f64);
            }
        }
        for worker in workers {
            let _ = worker.join();
        }
        cancelled = cancel.load(Ordering::Relaxed);
    }
    let completed = reference.is_some() as usize + points.len();
    emit_progress(completed, completed as f64 / total.max(1) as f64);

    points.sort_by_key(|p| (p.iy, p.ix));
    Ok(RobustnessResult {
        config,
        model_id,
        nominal_x,
        nominal_y,
        columns,
        reference_truth_table: reference,
        points,
        cancelled,
        duration_ms: started.elapsed().as_millis() as u64,
    })
}

/// Number of sweep points per slice when the sweep is split into `count`
/// slices; useful for choosing the size of a job array.
pub fn slice_sizes(config: &RobustnessConfig, count: usize) -> Vec<usize> {
    let n = config.num_points();
    (0..count.max(1))
        .map(|index| (0..n).filter(|i| i % count.max(1) == index).count())
        .collect()
}

// ---------------------------------------------------------------------------
// Run files
// ---------------------------------------------------------------------------

/// Description of a sweep axis in a run file, as the GUI shows it.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct AxisInfo {
    pub parameter: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    pub values: Vec<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nominal: Option<f64>,
}

impl AxisInfo {
    pub fn new(axis: &SweepAxis, nominal: Option<f64>) -> Self {
        let p = axis.parameter.as_str();
        let (name, unit): (String, Option<&str>) = match p.split_once(':') {
            _ if p == "geometry.cell_size" => ("Cell size (intercell distance)".into(), Some("nm")),
            _ if p == "geometry.dot_radius" => ("Quantum dot placement radius".into(), Some("nm")),
            _ if p == "geometry.dot_diameter" => ("Quantum dot diameter".into(), Some("nm")),
            Some(("geometry.layer_z", layer)) => {
                (format!("Z position of layer {}", layer), Some("nm"))
            }
            Some(("geometry.cell_offset_x", label)) => {
                (format!("X offset of cell {}", label), Some("nm"))
            }
            Some(("geometry.cell_offset_y", label)) => {
                (format!("Y offset of cell {}", label), Some("nm"))
            }
            _ => (
                p.split_once('.')
                    .map_or(p, |(_, name)| name)
                    .replace('_', " "),
                None,
            ),
        };
        AxisInfo {
            parameter: axis.parameter.clone(),
            name,
            unit: unit.map(str::to_string),
            values: axis.values.clone(),
            nominal,
        }
    }
}

/// Provenance of one slice that went into a (merged) run file.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct RunSliceInfo {
    pub index: usize,
    pub count: usize,
    pub points: usize,
    pub duration_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_id: Option<String>,
}

/// The `qcaforge-robustness-analysis` run file that the QCAForge
/// Robustness view saves and opens.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct RobustnessRun {
    pub format: String,
    pub version: u32,
    pub name: String,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    pub x: AxisInfo,
    pub y: Option<AxisInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_behavior: Option<ExpectedBehavior>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<RobustnessConfig>,
    pub columns: Vec<RobustnessColumn>,
    pub reference_truth_table: Option<TruthTableData>,
    pub points: Vec<SweepPoint>,
    pub cancelled: bool,
    /// Wall-clock time of the run (of the longest slice for a merged run).
    pub duration_ms: u64,
    /// Sum of the simulation times of all points.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_ms: Option<u64>,
    /// The slices a merged run was combined from.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub slices: Vec<RunSliceInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_files: Option<Vec<String>>,
}

impl RobustnessRun {
    pub fn from_result(name: &str, result: RobustnessResult) -> Self {
        let cpu_ms = result.points.iter().map(|p| p.duration_ms).sum();
        RobustnessRun {
            format: ROBUSTNESS_RUN_FORMAT.into(),
            version: ROBUSTNESS_RUN_VERSION,
            name: name.into(),
            created_at: chrono::Utc::now().to_rfc3339(),
            model_id: Some(result.model_id),
            x: AxisInfo::new(&result.config.x_axis, Some(result.nominal_x)),
            y: result
                .config
                .y_axis
                .as_ref()
                .map(|axis| AxisInfo::new(axis, result.nominal_y)),
            expected_behavior: Some(result.config.expected_behavior),
            columns: result.columns,
            reference_truth_table: result.reference_truth_table,
            points: result.points,
            cancelled: result.cancelled,
            duration_ms: result.duration_ms,
            cpu_ms: Some(cpu_ms),
            slices: Vec::new(),
            source_files: None,
            config: Some(result.config),
        }
    }

    /// Reads and checks a run file.
    pub fn from_json(contents: &str) -> Result<Self, String> {
        let run: RobustnessRun =
            serde_json::from_str(contents).map_err(|e| format!("Invalid run file: {}", e))?;
        if run.format != ROBUSTNESS_RUN_FORMAT {
            return Err("Not a QCAForge robustness analysis file".into());
        }
        Ok(run)
    }

    /// Number of points of the complete sweep grid.
    pub fn grid_size(&self) -> usize {
        self.x.values.len() * self.y.as_ref().map_or(1, |y| y.values.len())
    }

    /// Grid coordinates `(ix, iy)` without a point.
    pub fn missing_points(&self) -> Vec<(usize, usize)> {
        let present: HashSet<(usize, usize)> = self.points.iter().map(|p| (p.ix, p.iy)).collect();
        let ny = self.y.as_ref().map_or(1, |y| y.values.len());
        (0..ny)
            .flat_map(|iy| (0..self.x.values.len()).map(move |ix| (ix, iy)))
            .filter(|key| !present.contains(key))
            .collect()
    }
}

/// Summary of a merge.
#[derive(Debug, Clone, PartialEq)]
pub struct MergeReport {
    pub runs: usize,
    pub points: usize,
    pub duplicates: usize,
    pub missing: usize,
}

/// Combines the run files of the slices of one sweep (or any runs of the
/// same sweep that cover different points) into one run file. All runs must
/// describe the same sweep, model, columns and reference truth table. Points
/// present in several runs are kept once (the first occurrence).
pub fn merge_runs(
    name: &str,
    runs: Vec<RobustnessRun>,
) -> Result<(RobustnessRun, MergeReport), String> {
    let mut runs = runs.into_iter();
    let mut merged = runs.next().ok_or("No run files to merge")?;
    let mut count = 1;
    let slice_info = |run: &RobustnessRun| -> Vec<RunSliceInfo> {
        if !run.slices.is_empty() {
            return run.slices.clone();
        }
        let slice = run.config.as_ref().and_then(|c| c.slice);
        vec![RunSliceInfo {
            index: slice.map_or(0, |s| s.index),
            count: slice.map_or(1, |s| s.count),
            points: run.points.len(),
            duration_ms: run.duration_ms,
            host: None,
            job_id: None,
        }]
    };
    let mut slices = slice_info(&merged);
    let mut seen: HashSet<(usize, usize)> = merged.points.iter().map(|p| (p.ix, p.iy)).collect();
    let mut duplicates = 0;
    for run in runs {
        count += 1;
        let same_sweep = match (&merged.config, &run.config) {
            (Some(a), Some(b)) => a.same_sweep(b),
            (None, None) => {
                merged.x.values == run.x.values
                    && merged.y.as_ref().map(|y| &y.values) == run.y.as_ref().map(|y| &y.values)
            }
            _ => false,
        };
        if !same_sweep {
            return Err(format!(
                "Run '{}' is not a slice of the same sweep",
                run.name
            ));
        }
        if merged.model_id != run.model_id {
            return Err(format!(
                "Run '{}' uses a different simulation model",
                run.name
            ));
        }
        if merged.columns != run.columns {
            return Err(format!(
                "Run '{}' has different input/output cells",
                run.name
            ));
        }
        if merged.reference_truth_table.is_none() {
            merged.reference_truth_table = run.reference_truth_table.clone();
        } else if run.reference_truth_table.is_some()
            && run.reference_truth_table != merged.reference_truth_table
        {
            return Err(format!(
                "Run '{}' has a different reference truth table (was the nominal design changed?)",
                run.name
            ));
        }
        slices.extend(slice_info(&run));
        merged.cancelled |= run.cancelled;
        merged.duration_ms = merged.duration_ms.max(run.duration_ms);
        for point in run.points {
            if seen.insert((point.ix, point.iy)) {
                merged.points.push(point);
            } else {
                duplicates += 1;
            }
        }
    }
    merged.points.sort_by_key(|p| (p.iy, p.ix));
    merged.cpu_ms = Some(merged.points.iter().map(|p| p.duration_ms).sum());
    merged.name = name.into();
    merged.created_at = chrono::Utc::now().to_rfc3339();
    slices.sort_by_key(|s| (s.count, s.index));
    merged.slices = slices;
    if let Some(config) = merged.config.as_mut() {
        config.slice = None;
    }
    let report = MergeReport {
        runs: count,
        points: merged.points.len(),
        duplicates,
        missing: merged.missing_points().len(),
    };
    Ok((merged, report))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(values: &str) -> Vec<Option<char>> {
        values
            .chars()
            .map(|c| if c == '-' { None } else { Some(c) })
            .collect()
    }

    #[test]
    fn parses_parameter_ids() {
        assert_eq!("geometry.cell_size".parse(), Ok(SweepTarget::CellSize));
        assert_eq!("geometry.layer_z:1".parse(), Ok(SweepTarget::LayerZ(1)));
        assert_eq!(
            "geometry.cell_offset_y:O2".parse(),
            Ok(SweepTarget::CellOffset {
                label: "O2".into(),
                axis: 1
            })
        );
        assert_eq!(
            "model.relative_permitivity".parse(),
            Ok(SweepTarget::Model("relative_permitivity".into()))
        );
        assert_eq!(
            "clock.amplitude_max".parse(),
            Ok(SweepTarget::Clock("amplitude_max".into()))
        );
        assert!("geometry.layer_z".parse::<SweepTarget>().is_err());
        assert!("foo.bar".parse::<SweepTarget>().is_err());
        assert!("model.".parse::<SweepTarget>().is_err());
    }

    #[test]
    fn wire_accuracy_matches_script() {
        let wire = ExpectedBehavior::Wire;
        assert_eq!(wire.row_accuracy(&row("AAA")), 1.0);
        assert_eq!(wire.row_accuracy(&row("AAB")), 0.5);
        assert_eq!(wire.row_accuracy(&row("CD")), 1.0);
        assert_eq!(wire.row_accuracy(&row("A-")), 0.0);
        assert_eq!(wire.row_accuracy(&row("-B")), 1.0);
    }

    #[test]
    fn inverter_accuracy_matches_script() {
        let inverter = ExpectedBehavior::Inverter;
        assert_eq!(inverter.row_accuracy(&row("AxB")), 1.0);
        assert_eq!(inverter.row_accuracy(&row("BA")), 1.0);
        assert_eq!(inverter.row_accuracy(&row("DC")), 1.0);
        assert_eq!(inverter.row_accuracy(&row("AA")), 0.0);
    }

    #[test]
    fn majority_matches_script_table() {
        // Every entry of analyze_truth.py's _cmp_majority table.
        let table = [
            "AAAA", "AABA", "AACA", "ABAA", "ABBB", "ABCC", "ACAA", "ACBC", "ACCC", "BAAA", "BABB",
            "BACC", "BBAB", "BBBB", "BBCB", "BCAC", "BCBB", "BCCC", "CAAA", "CABC", "CACC", "CBAC",
            "CBBB", "CBCC", "CCAC", "CCBC", "CCCC",
        ];
        for entry in table {
            assert_eq!(
                ExpectedBehavior::Majority.row_accuracy(&row(entry)),
                1.0,
                "{}",
                entry
            );
        }
        assert_eq!(ExpectedBehavior::Majority.row_accuracy(&row("AABB")), 0.0);
        assert_eq!(ExpectedBehavior::Majority.row_accuracy(&row("AA-A")), 0.0);
    }

    #[test]
    fn sequential_circuits_match_script() {
        let memory = ExpectedBehavior::MemoryCell;
        assert_eq!(memory.row_accuracy(&row("BAA")), 1.0);
        assert_eq!(memory.row_accuracy(&row("BAB")), 0.0);
        assert_eq!(memory.row_accuracy(&row("-AB")), 1.0);

        let flipflop = ExpectedBehavior::Flipflop1;
        // (g1, flip) -> q, row order is [flip, g1, q]
        for (flip, g1, q) in [
            ('A', 'A', 'A'),
            ('B', 'A', 'A'),
            ('C', 'A', 'A'),
            ('A', 'B', 'B'),
            ('B', 'B', 'A'),
            ('C', 'B', 'A'),
            ('A', 'C', 'A'),
            ('B', 'C', 'C'),
            ('C', 'C', 'C'),
        ] {
            let r = vec![Some(flip), Some(g1), Some(q)];
            assert_eq!(flipflop.row_accuracy(&r), 1.0, "{:?}", r);
        }
    }

    #[test]
    fn ternary_flipflop_is_scored_sequentially() {
        // Columns T, R, Q: reset, toggle, toggle, hold, clear, toggle (0 stays 0), wrong hold.
        let table = TruthTableData {
            rows: vec![
                row("AAA"),
                row("BBB"),
                row("BBA"),
                row("ABA"),
                row("CBC"),
                row("BBD"),
                row("ABB"),
            ],
        };
        let (accuracy, rows) = score_table(ExpectedBehavior::TernaryFlipflop, &table, None, &[]);
        assert_eq!(rows, vec![1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 0.0]);
        assert!((accuracy - 6.0 / 7.0).abs() < 1e-12);
        assert_eq!(expected_ternary_flipflop('B', 'A', Some('C')), Some('A'));
        assert_eq!(expected_ternary_flipflop('B', 'B', None), None);
    }

    #[test]
    fn reference_accuracy_ignores_inputs_and_undefined_reference() {
        let reference = TruthTableData {
            rows: vec![row("AB"), row("BA"), row("A-")],
        };
        let table = TruthTableData {
            rows: vec![row("BB"), row("BB"), row("AA")],
        };
        let accuracy = table_accuracy(
            ExpectedBehavior::Reference,
            &table,
            Some(&reference),
            &[false, true],
        );
        assert!((accuracy - 2.0 / 3.0).abs() < 1e-12);
    }

    #[test]
    fn empty_table_is_fully_accurate() {
        let table = TruthTableData { rows: vec![] };
        assert_eq!(
            table_accuracy(ExpectedBehavior::Wire, &table, None, &[]),
            1.0
        );
    }

    #[test]
    fn column_validation() {
        assert!(ExpectedBehavior::Majority.validate_columns(4).is_ok());
        assert!(ExpectedBehavior::Majority.validate_columns(3).is_err());
        assert!(ExpectedBehavior::Wire.validate_columns(1).is_err());
    }

    fn load_example_raw(name: &str) -> Value {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../examples")
            .join(name);
        let contents = std::fs::read_to_string(path).unwrap();
        let mut file: Value = serde_json::from_str(&contents).unwrap();
        // Older examples predate these fields; the GUI always sends them.
        let settings = file["design"]["simulation_settings"]
            .as_object_mut()
            .unwrap();
        settings
            .entry("use_custom_input_sequence")
            .or_insert(Value::Bool(false));
        settings
            .entry("custom_input_sequence")
            .or_insert(Value::Array(vec![]));
        file["design"].take()
    }

    fn load_example(name: &str) -> QCADesign {
        serde_json::from_value(load_example_raw(name)).unwrap()
    }

    #[test]
    fn merge_missing_keeps_gui_fields() {
        let mut target = serde_json::json!({"layers": [{"z": 1}], "a": {"b": 2}});
        let source =
            serde_json::json!({"layers": [{"z": 0, "visible": true}], "a": {"b": 3, "c": 4}});
        merge_missing(&mut target, &source);
        assert_eq!(
            target,
            serde_json::json!({"layers": [{"z": 1, "visible": true}], "a": {"b": 2, "c": 4}})
        );
    }

    fn sweep_point_accuracy(design: &QCADesign, assignments: &[(&SweepTarget, f64)]) -> f64 {
        let variant = create_variant(design, assignments).unwrap();
        let simulation = simulate(&variant, &AtomicBool::new(false), |_| {})
            .unwrap()
            .unwrap();
        let thresholds = TruthTableThresholds {
            clock: 0.05,
            logical: 0.05,
            value: 0.8,
        };
        let table = compute_truth_table(&variant, &simulation, &HashMap::new(), thresholds);
        table_accuracy(ExpectedBehavior::Wire, &table, None, &[])
    }

    #[test]
    fn variants_change_the_right_fields() {
        let design = load_example("line.qcd");
        let cell_size = SweepTarget::CellSize;
        let offset = SweepTarget::CellOffset {
            label: "O2".into(),
            axis: 1,
        };
        let permittivity = SweepTarget::Model("relative_permitivity".into());
        let cycles = SweepTarget::Clock("num_cycles".into());

        assert_eq!(get_parameter(&design, &cell_size).unwrap(), 60.0);
        assert!((get_parameter(&design, &SweepTarget::DotRadius).unwrap() - 14.0).abs() < 1e-9);

        let variant = create_variant(
            &design,
            &[
                (&offset, 5.0),
                (&cell_size, 120.0),
                (&permittivity, 10.5),
                (&cycles, 2.4),
            ],
        )
        .unwrap();
        let o2 = |d: &QCADesign| {
            d.layers[0]
                .cells
                .iter()
                .find(|c| c.label.as_deref() == Some("O2"))
                .unwrap()
                .position
        };
        // Scaled first, then offset.
        let nominal_o2 = o2(&design);
        assert_eq!(
            o2(&variant),
            [nominal_o2[0] * 2.0, nominal_o2[1] * 2.0 + 5.0]
        );
        assert_eq!(get_parameter(&variant, &cell_size).unwrap(), 120.0);
        assert_eq!(get_parameter(&variant, &permittivity).unwrap(), 10.5);
        // Whole-number options stay integers so the model can parse them.
        let settings = &variant.simulation_settings.simulation_model_settings["icha_model"];
        assert_eq!(
            settings.clock_generator_settings["num_cycles"],
            Value::from(2u64)
        );
        prepare_simulation(&variant).unwrap();

        let radius = create_variant(&design, &[(&SweepTarget::DotRadius, 20.0)]).unwrap();
        assert!((get_parameter(&radius, &SweepTarget::DotRadius).unwrap() - 20.0).abs() < 1e-9);

        assert!(create_variant(
            &design,
            &[(
                &SweepTarget::CellOffset {
                    label: "missing".into(),
                    axis: 0
                },
                1.0
            )]
        )
        .is_err());
        assert!(create_variant(&design, &[(&SweepTarget::Model("nope".into()), 1.0)]).is_err());
    }

    #[test]
    fn wire_example_is_accurate_at_nominal_geometry() {
        let design = load_example("line.qcd");
        let accuracy = sweep_point_accuracy(&design, &[(&SweepTarget::CellSize, 60.0)]);
        assert_eq!(accuracy, 1.0);
    }

    /// Parity check against `analyze_truth.py`: scores the `.qcs` files
    /// listed in `ROBUSTNESS_PARITY_QCS` (separated by `;`) as a wire.
    #[test]
    #[ignore]
    fn parity_with_script() {
        let files = std::env::var("ROBUSTNESS_PARITY_QCS").unwrap();
        let thresholds = TruthTableThresholds {
            clock: 0.05,
            logical: 0.05,
            value: 0.8,
        };
        for path in files.split(';') {
            let (design, simulation) =
                crate::simulation::file::read_from_file(File::open(path).unwrap()).unwrap();
            let table = compute_truth_table(&design, &simulation, &HashMap::new(), thresholds);
            let accuracy = table_accuracy(ExpectedBehavior::Wire, &table, None, &[]);
            println!(
                "PARITY {} {:.6}",
                std::path::Path::new(path)
                    .file_name()
                    .unwrap()
                    .to_string_lossy(),
                accuracy
            );
        }
    }

    /// Writes a displacement sweep of the line example to
    /// `ROBUSTNESS_PARITY_DIR` and prints the Rust accuracy of every point.
    #[test]
    #[ignore]
    fn generate_parity_files() {
        let dir = std::env::var("ROBUSTNESS_PARITY_DIR").unwrap();
        let design = load_example("line.qcd");
        let config: RobustnessConfig = serde_json::from_value(serde_json::json!({
            "x_axis": { "parameter": "geometry.cell_size", "values": [60.0, 90.0] },
            "y_axis": { "parameter": "geometry.cell_offset_y:O2", "values": [0.0, 8.0, 12.0, 16.0, 20.0, 30.0] },
            "expected_behavior": "wire",
            "thresholds": { "clock": 0.05, "logical": 0.05, "value": 0.8 },
            "output_dir": dir,
            "base_name": "line",
        }))
        .unwrap();
        let context = SweepContext {
            nominal_raw: load_example_raw("line.qcd"),
            x_target: config.x_axis.parameter.parse().unwrap(),
            y_target: Some(config.y_axis.as_ref().unwrap().parameter.parse().unwrap()),
            nominal: design,
            delays: HashMap::new(),
            reference: None,
            output_columns: vec![],
            app_version: "test".into(),
            config,
        };
        for (ix, x) in [60.0, 90.0].into_iter().enumerate() {
            for (iy, y) in [0.0, 8.0, 12.0, 16.0, 20.0, 30.0].into_iter().enumerate() {
                let job = SweepJob {
                    ix,
                    iy,
                    x,
                    y: Some(y),
                };
                let point = run_job(&context, &job, &AtomicBool::new(false), |_| {}).unwrap();
                assert!(point.error.is_none(), "{:?}", point.error);
                let file = point.simulation_file.unwrap();
                println!(
                    "PARITY {} {:.6}",
                    std::path::Path::new(&file)
                        .file_name()
                        .unwrap()
                        .to_string_lossy(),
                    point.accuracy.unwrap()
                );
            }
        }
    }

    #[test]
    fn slices_partition_the_grid() {
        let slice: SweepSlice = "2/3".parse().unwrap();
        assert_eq!(slice, SweepSlice { index: 2, count: 3 });
        assert!("3/3".parse::<SweepSlice>().is_err());
        assert!("1/0".parse::<SweepSlice>().is_err());
        assert!("x".parse::<SweepSlice>().is_err());
        let config: RobustnessConfig = serde_json::from_value(serde_json::json!({
            "x_axis": { "parameter": "geometry.cell_size", "values": [1, 2, 3, 4, 5] },
            "y_axis": { "parameter": "geometry.dot_radius", "values": [1, 2] },
            "expected_behavior": "wire",
            "thresholds": { "clock": 0.05, "logical": 0.05, "value": 0.8 },
        }))
        .unwrap();
        assert_eq!(config.num_points(), 10);
        assert_eq!(slice_sizes(&config, 3), vec![4, 3, 3]);
        assert_eq!(slice_sizes(&config, 16).iter().sum::<usize>(), 10);
    }

    fn line_sweep_config(slice: Option<SweepSlice>) -> RobustnessConfig {
        serde_json::from_value(serde_json::json!({
            "x_axis": { "parameter": "geometry.cell_size", "values": [40.0, 60.0, 80.0] },
            "y_axis": { "parameter": "geometry.dot_radius", "values": [10.0, 14.0] },
            "expected_behavior": "wire",
            "thresholds": { "clock": 0.05, "logical": 0.05, "value": 0.8 },
            "max_threads": 2,
            "slice": slice,
        }))
        .unwrap()
    }

    fn run_line(config: RobustnessConfig, completed: Vec<SweepPoint>) -> RobustnessResult {
        run_sweep(
            &NoObserver,
            load_example_raw("line.qcd"),
            load_example("line.qcd"),
            config,
            Arc::new(AtomicBool::new(false)),
            SweepOptions {
                completed_points: completed,
                ..Default::default()
            },
        )
        .unwrap()
    }

    #[test]
    fn sliced_and_merged_sweep_equals_full_sweep() {
        let full = RobustnessRun::from_result("full", run_line(line_sweep_config(None), vec![]));
        assert_eq!(full.points.len(), 6);
        assert!(full.missing_points().is_empty());

        let parts: Vec<RobustnessRun> = (0..3)
            .map(|index| {
                let slice = SweepSlice { index, count: 3 };
                let result = run_line(line_sweep_config(Some(slice)), vec![]);
                assert_eq!(result.points.len(), 2);
                let run = RobustnessRun::from_result("part", result);
                // Round trip through the file format.
                RobustnessRun::from_json(&serde_json::to_string(&run).unwrap()).unwrap()
            })
            .collect();
        let incomplete = merge_runs("incomplete", parts[..2].to_vec()).unwrap().1;
        assert_eq!(incomplete.missing, 2);

        let (merged, report) = merge_runs("merged", parts).unwrap();
        assert_eq!(
            report,
            MergeReport {
                runs: 3,
                points: 6,
                duplicates: 0,
                missing: 0
            }
        );
        assert_eq!(merged.slices.len(), 3);
        assert!(merged.config.as_ref().unwrap().slice.is_none());
        for (a, b) in full.points.iter().zip(&merged.points) {
            assert_eq!((a.ix, a.iy), (b.ix, b.iy));
            assert_eq!(a.accuracy, b.accuracy);
            assert_eq!(a.truth_table, b.truth_table);
        }

        let mut other = full.clone();
        other.config.as_mut().unwrap().expected_behavior = ExpectedBehavior::Inverter;
        assert!(merge_runs("bad", vec![full.clone(), other]).is_err());
    }

    #[test]
    fn completed_points_are_not_simulated_again() {
        let slice = Some(SweepSlice { index: 0, count: 2 });
        let first = run_line(line_sweep_config(slice), vec![]);
        let mut kept = first.points[..1].to_vec();
        kept[0].duration_ms = 123_456;
        // A point outside the slice is ignored.
        kept.push(SweepPoint {
            ix: 1,
            iy: 0,
            ..first.points[0].clone()
        });
        let resumed = run_line(line_sweep_config(slice), kept);
        assert_eq!(resumed.points.len(), first.points.len());
        assert_eq!(resumed.points[0].duration_ms, 123_456);
        for (a, b) in first.points.iter().zip(&resumed.points) {
            assert_eq!(a.accuracy, b.accuracy);
        }
    }

    #[test]
    fn value_formatting() {
        assert_eq!(format_value(60.0), "60");
        assert_eq!(format_value(14.5), "14.5");
        assert_eq!(format_value(0.1 + 0.2), "0.3");
        assert_eq!(format_value(1e-6), "0.000001");
    }
}
