use super::{CellType, QCACellArchitecture, SimulationModelTrait};
use crate::objects::cell::{
    dot_probability_distribution_to_polarization, polarization_to_dot_probability_distribution,
    QCACell, QCACellIndex,
};
use crate::objects::layer::QCALayer;
use crate::simulation::model::{ClockGeneratorSettingsTrait, SimulationModelSettingsTrait};
use crate::simulation::settings::{InputDescriptor, OptionsEntry, OptionsList};
use serde::{Deserialize, Serialize};
use serde_inline_default::serde_inline_default;
use std::{collections::HashMap, mem};

struct BistableNeighbor {
    cell_index: QCACellIndex,
    // kink_energy[k][l] is the coupling between this cell's polarization
    // component k and the neighbor's polarization component l.
    kink_energy: Vec<Vec<f64>>,
}

pub struct BistableModel {
    clock_states: [f64; 4],
    input_states: Vec<f64>,
    index_cells_static_map: HashMap<QCACellIndex, QCACell>,
    index_cells_read_map: HashMap<QCACellIndex, QCACell>,
    index_cells_write_map: HashMap<QCACellIndex, QCACell>,
    cell_input_map: HashMap<QCACellIndex, usize>,
    layer_map: HashMap<usize, QCALayer>,
    cell_architectures_map: HashMap<String, QCACellArchitecture>,
    neighborhood_map: HashMap<QCACellIndex, Vec<BistableNeighbor>>,
    model_settings: BistableModelSettings,
    clock_settings: BistableClockGeneratorSettings,
}

#[serde_inline_default]
#[derive(Serialize, Deserialize, Clone)]
pub struct BistableModelSettings {
    #[serde_inline_default(1000)]
    max_iterations: usize,

    #[serde_inline_default(1e-3)]
    convergence_tolerance: f64,

    // Covers axis-aligned *and* diagonal nearest neighbors on a 60nm grid
    // (diagonal spacing ~84.85nm) without reaching the next ring of cells
    // two hops away (120nm). A radius large enough to pull in those more
    // distant cells adds competing, weaker couplings that fight the direct
    // neighbor's signal and drag cells towards an ambiguous, unsaturated
    // state instead of cleanly latching to an input.
    #[serde_inline_default(90.0)]
    neighborhood_radius: f64,

    #[serde_inline_default(12.9)]
    relative_permitivity: f64,
}

#[serde_inline_default]
#[derive(Serialize, Deserialize, Clone)]
pub struct BistableClockGeneratorSettings {
    #[serde_inline_default(1)]
    num_cycles: usize,

    // Clock amplitudes are compared directly against kink energies, which
    // are real Coulomb interaction energies in joules (~1e-19 to 1e-23 J for
    // nanometer-scale QCA cells) - not the arbitrary small/large numbers
    // used previously, which were many orders of magnitude off and left
    // every cell effectively unable to switch.
    #[serde_inline_default(1e-25)]
    amplitude_min: f64,

    #[serde_inline_default(1e-19)]
    amplitude_max: f64,

    #[serde_inline_default(0)]
    extra_periods: usize,

    #[serde_inline_default(20)]
    samples_per_input: usize,
}

impl BistableModelSettings {
    pub fn new() -> Self {
        serde_json::from_str::<BistableModelSettings>("{}".into()).unwrap()
    }
}

impl SimulationModelSettingsTrait for BistableModelSettings {
    fn get_max_iterations(&self) -> usize {
        self.max_iterations
    }
    fn get_convergence_tolerance(&self) -> f64 {
        self.convergence_tolerance
    }
}

impl BistableClockGeneratorSettings {
    pub fn new() -> Self {
        serde_json::from_str::<BistableClockGeneratorSettings>("{}".into()).unwrap()
    }
}

impl ClockGeneratorSettingsTrait for BistableClockGeneratorSettings {
    fn get_num_cycles(&self) -> usize {
        self.num_cycles
    }
    fn get_amplitude_min(&self) -> f64 {
        self.amplitude_min
    }
    fn get_amplitude_max(&self) -> f64 {
        self.amplitude_max
    }
    fn get_extra_periods(&self) -> usize {
        self.extra_periods
    }
    fn get_samples_per_input(&self) -> usize {
        self.samples_per_input
    }
}

impl BistableModel {
    pub fn new() -> Self {
        BistableModel {
            clock_states: [0.0, 0.0, 0.0, 0.0],
            input_states: vec![],
            index_cells_static_map: HashMap::new(),
            index_cells_read_map: HashMap::new(),
            index_cells_write_map: HashMap::new(),
            cell_input_map: HashMap::new(),
            layer_map: HashMap::new(),
            cell_architectures_map: HashMap::new(),
            neighborhood_map: HashMap::new(),
            model_settings: BistableModelSettings::new(),
            clock_settings: BistableClockGeneratorSettings::new(),
        }
    }

    fn cell_distance(cell_a: &QCACell, cell_b: &QCACell) -> f64 {
        ((cell_a.position[0] - cell_b.position[0]).powf(2.0)
            + (cell_a.position[1] - cell_b.position[1]).powf(2.0))
        .sqrt()
    }

    // Position of a single dot of `cell`, in the same 2D coordinate space as
    // `cell.position`, accounting for the cell's rotation and the dot layout
    // of its architecture.
    fn get_dot_position(
        dot_index: usize,
        cell: &QCACell,
        architecture: &QCACellArchitecture,
    ) -> [f64; 2] {
        let [x, y] = architecture.dot_positions[dot_index];
        [
            cell.position[0] + x * cell.rotation.cos() - y * cell.rotation.sin(),
            cell.position[1] + y * cell.rotation.cos() + x * cell.rotation.sin(),
        ]
    }

    fn dot_distance(a: &[f64; 2], b: &[f64; 2]) -> f64 {
        ((a[0] - b[0]).powf(2.0) + (a[1] - b[1]).powf(2.0)).sqrt()
    }

    /// Kink energy between polarization component `component_a` of `cell_a`
    /// and polarization component `component_b` of `cell_b`. Generalizes the
    /// classic 2-state (4-dot) bistable kink energy to cells with any
    /// dot_count by deriving each dot's charge dipole (the difference between
    /// the dot's occupation when the component is fully +1 versus fully -1)
    /// directly from `polarization_to_dot_probability_distribution`, then
    /// summing the pairwise Coulomb interaction over every dot pair using the
    /// actual dot geometry of each cell's architecture.
    ///
    /// Both cells' contributions MUST be built the same way (here, the
    /// symmetric pos-minus-neg dipole) rather than one being measured against
    /// the pos-minus-neg dipole and the other against the pos-minus-average
    /// baseline: the latter leaves a residual "how does the +1 state differ
    /// from no polarization at all" term on the asymmetric side that isn't
    /// present on the other, which is fine for a single-component cell
    /// (component_a == component_b, where that residual is orthogonal to the
    /// other cell's dipole and cancels) but silently breaks the physical
    /// symmetry of the Coulomb interaction - kink_energy(a, ca, b, cb) must
    /// equal kink_energy(b, cb, a, ca) - for cross-component terms
    /// (component_a != component_b), which multi-component (tri-state+)
    /// architectures rely on. That asymmetry was making cells fed only by
    /// weak cross-component neighbors settle on a spurious, input-independent
    /// polarization instead of tracking their actual neighbors.
    fn determine_kink_energy(
        cell_a: &QCACell,
        arch_a: &QCACellArchitecture,
        component_a: usize,
        cell_b: &QCACell,
        arch_b: &QCACellArchitecture,
        component_b: usize,
        permitivity: f64,
    ) -> f64 {
        const E_CHARGE: f64 = 1.602_176_634e-19;
        const FOUR_PI_EPSILON: f64 = 1.11265005597565794635320037482e-10;

        let n_a = arch_a.dot_count as usize;
        let n_b = arch_b.dot_count as usize;
        let num_components_a = n_a / 4;
        let num_components_b = n_b / 4;

        let occupation = |num_components: usize, component: usize, sign: f64| {
            let mut polarization = vec![0.0; num_components];
            polarization[component] = sign;
            polarization_to_dot_probability_distribution(&polarization)
        };

        // Raw pairwise Coulomb sum for one specific (component_a, component_b)
        // pair, before any sign/magnitude correction.
        let raw_energy = |component_a: usize, component_b: usize| -> f64 {
            let occ_a_pos = occupation(num_components_a, component_a, 1.0);
            let occ_a_neg = occupation(num_components_a, component_a, -1.0);
            let occ_b_pos = occupation(num_components_b, component_b, 1.0);
            let occ_b_neg = occupation(num_components_b, component_b, -1.0);

            let mut energy: f64 = 0.0;
            for i in 0..n_a {
                let dipole_a = occ_a_pos[i] - occ_a_neg[i];
                if dipole_a == 0.0 {
                    continue;
                }
                let pos_a = Self::get_dot_position(i, cell_a, arch_a);

                for j in 0..n_b {
                    let dipole_b = occ_b_pos[j] - occ_b_neg[j];
                    if dipole_b == 0.0 {
                        continue;
                    }
                    let pos_b = Self::get_dot_position(j, cell_b, arch_b);
                    let dist = 1e-9 * Self::dot_distance(&pos_a, &pos_b);

                    energy += dipole_a * dipole_b * E_CHARGE * E_CHARGE / dist;
                }
            }
            energy
        };

        let energy = if component_a == component_b {
            // A cell's own component driving its own component is, by
            // definition, what makes a chain of same-architecture cells behave
            // as a *wire*, and what lets a multi-way cell (like this tri-state
            // architecture's two polarization components) act as a fair,
            // symmetric multi-valued signal rather than favoring whichever
            // value happens to sit on the geometrically "luckier" pair of dots:
            //
            // Whether matching polarization is energetically favorable
            // (same-sign, "pass-through") or unfavorable (opposite-sign,
            // "alternating") is a genuine physical property of each
            // component's dot geometry, not something that can be assumed
            // uniform across components: this architecture's axis-aligned
            // component (dots 0/2/4/6) has each cell's "hot" dot sitting
            // directly on the line to an axis-neighbor, which does behave
            // like the classic 4-dot cell (alternating). The diagonal
            // component (dots 1/3/5/7) spreads its charge across two
            // off-axis dots on each side instead, which is not the same
            // interaction and does not have to (and empirically does not,
            // matching the ICHA reference model) share the classic cell's
            // sign - it passes a signal through unchanged instead of
            // alternating it. So each component's *sign* here must come
            // from its own raw pairwise sum, not be coerced to match the
            // others.
            //
            // The *magnitude* of that raw sum does still differ between
            // components (axis-aligned dots sit closer to an axis-aligned
            // neighbor than diagonal dots do), which would let a single vote
            // cast on one component outweigh two votes cast on the other in
            // any circuit - like a majority gate - where different input
            // lines happen to use different components. Normalize only the
            // magnitude to the strongest among every component this
            // architecture has (falling back to this pair's own magnitude
            // when it only has one, i.e. the classic cell), while keeping
            // each component's own sign, so every signal level latches
            // equally strongly without altering which configuration that
            // component treats as favorable.
            let num_components = num_components_a.min(num_components_b);
            let max_magnitude = (0..num_components)
                .map(|c| raw_energy(c, c).abs())
                .fold(0.0_f64, f64::max);
            let own_energy = raw_energy(component_a, component_a);
            if own_energy == 0.0 {
                0.0
            } else {
                -own_energy.signum() * max_magnitude
            }
        } else {
            raw_energy(component_a, component_b)
        };

        -(1.0 / (FOUR_PI_EPSILON * permitivity)) * energy
    }
}

impl SimulationModelTrait for BistableModel {
    fn get_name(&self) -> String {
        "Bistable".into()
    }

    fn get_unique_id(&self) -> String {
        "bistable".into()
    }

    fn get_model_settings(&self) -> Box<dyn SimulationModelSettingsTrait> {
        Box::new(self.model_settings.clone()) as Box<dyn SimulationModelSettingsTrait>
    }

    fn get_clock_generator_settings(&self) -> Box<dyn ClockGeneratorSettingsTrait> {
        Box::new(self.clock_settings.clone()) as Box<dyn ClockGeneratorSettingsTrait>
    }

    fn get_model_options_list(&self) -> OptionsList {
        vec![
            OptionsEntry::Input {
                unique_id: "max_iterations".to_string(),
                name: "Maximum iterations".to_string(),
                description:
                    "The maximum number of iterations used for simulation convergence check"
                        .to_string(),
                descriptor: InputDescriptor::NumberInput {
                    min: Some(1.0),
                    max: None,
                    unit: None,
                    whole_num: true,
                },
            },
            OptionsEntry::Input {
                unique_id: "convergence_tolerance".to_string(),
                name: "Convergence tolerance".to_string(),
                description: "Tolerance for simulation convergence check".to_string(),
                descriptor: InputDescriptor::NumberInput {
                    min: Some(0.0),
                    max: Some(1.0),
                    unit: None,
                    whole_num: false,
                },
            },
            OptionsEntry::Input {
                unique_id: "neighborhood_radius".to_string(),
                name: "Radius of effect".to_string(),
                description: "Radius of effect for neighbouring cells".to_string(),
                descriptor: InputDescriptor::NumberInput {
                    min: Some(0.0),
                    max: None,
                    unit: Some("nm".into()),
                    whole_num: false,
                },
            },
            OptionsEntry::Input {
                unique_id: "relative_permitivity".to_string(),
                name: "Relative permitivity".to_string(),
                description: "Relative permitivity of the relative medium".to_string(),
                descriptor: InputDescriptor::NumberInput {
                    min: Some(0.0),
                    max: None,
                    unit: None,
                    whole_num: false,
                },
            },
        ]
    }

    fn get_clock_generator_options_list(&self) -> OptionsList {
        vec![
            OptionsEntry::Input {
                unique_id: "num_cycles".to_string(),
                name: "Number of cycles".to_string(),
                description: "The number of repeating clock cycles to run".to_string(),
                descriptor: InputDescriptor::NumberInput {
                    min: Some(1.0),
                    max: None,
                    unit: None,
                    whole_num: true,
                },
            },
            OptionsEntry::Input {
                unique_id: "amplitude_min".to_string(),
                name: "Minimum amplitude".to_string(),
                description: "The minimum value of the clock signal".to_string(),
                descriptor: InputDescriptor::NumberInput {
                    min: None,
                    max: None,
                    unit: None,
                    whole_num: false,
                },
            },
            OptionsEntry::Input {
                unique_id: "amplitude_max".to_string(),
                name: "Maximum amplitude".to_string(),
                description: "The maximum value of the clock signal".to_string(),
                descriptor: InputDescriptor::NumberInput {
                    min: None,
                    max: None,
                    unit: None,
                    whole_num: false,
                },
            },
            OptionsEntry::Input {
                unique_id: "extra_periods".to_string(),
                name: "Extra periods".to_string(),
                description: "Extra clock periods at the end to account for delays".to_string(),
                descriptor: InputDescriptor::NumberInput {
                    min: Some(0.0),
                    max: None,
                    unit: None,
                    whole_num: true,
                },
            },
            OptionsEntry::Input {
                unique_id: "samples_per_input".to_string(),
                name: "Samples per input".to_string(),
                description: "Number of samples to be simulated for each input combination"
                    .to_string(),
                descriptor: InputDescriptor::NumberInput {
                    min: Some(1.0),
                    max: None,
                    unit: None,
                    whole_num: true,
                },
            },
        ]
    }

    fn serialize_model_settings(&self) -> Result<String, String> {
        match serde_json::to_string(&self.model_settings) {
            Ok(res) => Ok(res),
            Err(err) => Err(err.to_string()),
        }
    }

    fn deserialize_model_settings(&mut self, settings_str: &String) -> Result<(), String> {
        match serde_json::from_str::<BistableModelSettings>(settings_str) {
            Ok(res) => {
                self.model_settings = res;
                Ok(())
            }
            Err(err) => Err(err.to_string()),
        }
    }

    fn serialize_clock_generator_settings(&self) -> Result<String, String> {
        match serde_json::to_string(&self.clock_settings) {
            Ok(res) => Ok(res),
            Err(err) => Err(err.to_string()),
        }
    }

    fn deserialize_clock_generator_settings(
        &mut self,
        settings_str: &String,
    ) -> Result<(), String> {
        match serde_json::from_str::<BistableClockGeneratorSettings>(settings_str) {
            Ok(res) => {
                self.clock_settings = res;
                Ok(())
            }
            Err(err) => Err(err.to_string()),
        }
    }

    fn initiate(
        &mut self,
        layers: Box<Vec<QCALayer>>,
        qca_architetures_map: HashMap<String, QCACellArchitecture>,
    ) {
        self.index_cells_static_map.clear();
        self.index_cells_read_map.clear();
        self.cell_input_map.clear();
        self.layer_map.clear();
        self.cell_architectures_map = qca_architetures_map;

        let mut input_count = 0;
        layers.iter().enumerate().for_each(|(i, layer)| {
            self.layer_map.insert(i, layer.clone());
            layer.cells.iter().enumerate().for_each(|(j, cell)| {
                let cell_index = QCACellIndex::new(i, j);
                match cell.typ {
                    CellType::Input | CellType::Fixed => {
                        self.index_cells_static_map
                            .insert(cell_index.clone(), cell.clone());
                        if cell.typ == CellType::Input {
                            self.cell_input_map.insert(cell_index.clone(), input_count);
                            input_count += 1;
                        }
                    }
                    CellType::Normal | CellType::Output => {
                        self.index_cells_read_map
                            .insert(cell_index.clone(), cell.clone());
                    }
                }
            })
        });
        self.index_cells_write_map = self.index_cells_read_map.clone();

        let permitivity = self.model_settings.relative_permitivity;
        let radius = self.model_settings.neighborhood_radius;

        let all_cells_iter = self
            .index_cells_static_map
            .iter()
            .chain(self.index_cells_read_map.iter());

        let mut neighborhood_map: HashMap<QCACellIndex, Vec<BistableNeighbor>> = HashMap::new();

        all_cells_iter.clone().for_each(|(index_i, cell_i)| {
            let arch_i = &self.cell_architectures_map
                [&self.layer_map[&index_i.layer].cell_architecture_id];
            let num_components_i = arch_i.dot_count as usize / 4;

            all_cells_iter.clone().for_each(|(index_j, cell_j)| {
                if index_i == index_j || BistableModel::cell_distance(cell_i, cell_j) > radius {
                    return;
                }

                let arch_j = &self.cell_architectures_map
                    [&self.layer_map[&index_j.layer].cell_architecture_id];
                let num_components_j = arch_j.dot_count as usize / 4;

                let kink_energy: Vec<Vec<f64>> = (0..num_components_i)
                    .map(|k| {
                        (0..num_components_j)
                            .map(|l| {
                                BistableModel::determine_kink_energy(
                                    cell_i, arch_i, k, cell_j, arch_j, l, permitivity,
                                )
                            })
                            .collect()
                    })
                    .collect();

                neighborhood_map
                    .entry(index_i.clone())
                    .or_insert_with(Vec::new)
                    .push(BistableNeighbor {
                        cell_index: index_j.clone(),
                        kink_energy,
                    });
            });
        });

        self.neighborhood_map = neighborhood_map;
    }

    fn pre_calculate(&mut self, clock_states: &[f64; 4], input_states: &Vec<f64>) {
        self.clock_states = clock_states.clone();
        self.input_states = input_states.clone();
        mem::swap(
            &mut self.index_cells_read_map,
            &mut self.index_cells_write_map,
        );
        self.index_cells_write_map = self.index_cells_read_map.clone();

        let layer_map = &self.layer_map;
        let cell_architectures_map = &self.cell_architectures_map;
        let cell_input_map = &self.cell_input_map;

        self.index_cells_static_map
            .iter_mut()
            .for_each(|(index, cell)| {
                if cell.typ == CellType::Input {
                    let layer = layer_map.get(&index.layer).unwrap();
                    let architecture = cell_architectures_map
                        .get(&layer.cell_architecture_id)
                        .unwrap();
                    let num_components = architecture.dot_count as usize / 4;

                    let input_index = *cell_input_map.get(index).unwrap();
                    let input = input_states[(num_components * input_index)
                        ..(num_components * input_index + num_components)]
                        .to_vec();
                    cell.dot_probability_distribution =
                        polarization_to_dot_probability_distribution(&input);
                }
            });
    }

    fn calculate(&mut self, cell_ind: QCACellIndex) -> bool {
        let cell_options = self.index_cells_write_map.get(&cell_ind);
        if cell_options.is_none() {
            return true;
        }
        let mut cell = cell_options.unwrap().clone();

        let layer = self.layer_map.get(&cell_ind.layer).unwrap();
        let architecture = self
            .cell_architectures_map
            .get(&layer.cell_architecture_id)
            .unwrap();
        let num_components = architecture.dot_count as usize / 4;

        let mut effective_field = vec![0.0; num_components];

        if let Some(neighbors) = self.neighborhood_map.get(&cell_ind) {
            for neighbour in neighbors {
                let neighbour_cell = {
                    if let Some(neighbour_cell) =
                        self.index_cells_read_map.get(&neighbour.cell_index)
                    {
                        neighbour_cell
                    } else if let Some(neighbour_cell) =
                        self.index_cells_static_map.get(&neighbour.cell_index)
                    {
                        neighbour_cell
                    } else {
                        panic!("Unknown neighbour");
                    }
                };
                let neighbour_polarization = dot_probability_distribution_to_polarization(
                    &neighbour_cell.dot_probability_distribution,
                );

                for k in 0..num_components {
                    for l in 0..neighbour_polarization.len() {
                        effective_field[k] +=
                            neighbour.kink_energy[k][l] * neighbour_polarization[l];
                    }
                }
            }
        }

        let clock_index = (cell.clock_phase_shift.rem_euclid(360.0) / 90.0) as usize;
        let clock_value = 2.0 * self.clock_states[clock_index];

        // The components together share a single charge budget (the sum of
        // their magnitudes cannot exceed 1.0, enforced by
        // polarization_to_dot_probability_distribution). A cell with several
        // components (e.g. this architecture's tri-state cells) represents a
        // genuinely multi-valued signal - each component/sign combination is
        // a distinct, mutually exclusive state the cell can settle into, the
        // way a real bistable cell commits to one energy well rather than
        // averaging between them. So when several neighbors pull in favor of
        // different components (e.g. a majority-gate junction fed by input
        // lines that don't all happen to use the same component), the cell
        // must commit to whichever single component has the strongest net
        // pull, not blend proportionally across all of them: proportional
        // blending can't ever produce a definite k-out-of-n majority decision
        // - two weaker votes agreeing on one component and a single stronger
        // vote on another would settle on a fixed in-between ratio rather
        // than committing to the majority side. Saturate the combined L1
        // magnitude with the same tanh-like curve the original scalar model
        // used (representing overall confidence across every component, so a
        // near-tied field still ends up weakly polarized rather than
        // snapping hard to a coin flip), then apply all of that saturated
        // magnitude to the single dominant component and zero the rest.
        let polar_math: Vec<f64> = effective_field.iter().map(|f| f / clock_value).collect();
        let l1: f64 = polar_math.iter().map(|v| v.abs()).sum();

        let saturated_l1 = if l1 > 1000.0 {
            1.0 - 1e-9
        } else if l1 < 0.001 {
            l1
        } else {
            l1 / f64::sqrt(1.0 + l1 * l1)
        };

        let mut new_polarization = vec![0.0; num_components];
        if l1 > 0.0 {
            let (dominant, &dominant_value) = polar_math
                .iter()
                .enumerate()
                .max_by(|(_, a), (_, b)| a.abs().partial_cmp(&b.abs()).unwrap())
                .unwrap();
            new_polarization[dominant] = dominant_value.signum() * saturated_l1;
        }

        let new_dot_probability = polarization_to_dot_probability_distribution(&new_polarization);
        let mut stable = true;
        for i in 0..new_dot_probability.len() {
            if (new_dot_probability[i] - cell.dot_probability_distribution[i]).abs()
                > self.model_settings.convergence_tolerance
            {
                stable = false;
            }
        }
        cell.dot_probability_distribution = new_dot_probability;

        self.index_cells_write_map.insert(cell_ind, cell);

        stable
    }

    fn get_states(&self, cell_ind: &QCACellIndex) -> Vec<f64> {
        if let Some(c) = self.index_cells_write_map.get(cell_ind) {
            return c.dot_probability_distribution.clone();
        }
        if let Some(c) = self.index_cells_read_map.get(cell_ind) {
            return c.dot_probability_distribution.clone();
        }
        if let Some(c) = self.index_cells_static_map.get(cell_ind) {
            return c.dot_probability_distribution.clone();
        }
        panic!("Cell not found");
    }
}
