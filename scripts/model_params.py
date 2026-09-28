"""Model parameters exported from QCAForge and design parameter sweeps.

QCAForge can export every parameter of a design's simulation model
(Design view > model menu > "Export model parameters..." or the
"Export parameters..." button in the Model / Clock generator settings
dialogs). The exported JSON looks like::

    {
      "format": "qcaforge-model-parameters",
      "model": {"id": "icha_model", "name": "ICHA"},
      "parameters": [
        {"id": "model.relative_permitivity", "value": 12.9, "unit": null,
         "whole_num": false, ...},
        {"id": "clock.amplitude_max", ...},
        {"id": "geometry.cell_size", "value": 60, "unit": "nm", ...},
        ...
      ],
      "model_settings": {...},
      "clock_generator_settings": {...},
      "cell_architectures": {...}
    }

Parameter ids are shared with QCAForge's robustness analysis:

    geometry.cell_size              cell side length; cell positions scale with it
    geometry.dot_radius             distance of the quantum dots from the cell center
    geometry.dot_diameter           quantum dot diameter
    geometry.layer_z:<layer>        z-position of a layer
    geometry.cell_offset_x:<label>  displacement of labelled cells along x
    geometry.cell_offset_y:<label>  displacement of labelled cells along y
    model.<key>                     model setting of the selected model
    clock.<key>                     clock generator setting of the selected model
"""

import copy
import json
import math

PARAMETERS_FORMAT = 'qcaforge-model-parameters'


def load_parameters(filename: str) -> dict:
    """Loads a parameter file exported from QCAForge."""
    with open(filename, 'r', encoding='utf-8') as f:
        exported = json.load(f)
    if exported.get('format') != PARAMETERS_FORMAT:
        raise ValueError(f'{filename} is not a QCAForge model parameter export')
    return exported


def parameter_values(exported: dict) -> dict[str, float]:
    """Maps every exported parameter id to its value."""
    return {p['id']: p['value'] for p in exported['parameters']}


def whole_number_parameters(exported: dict) -> set[str]:
    return {p['id'] for p in exported['parameters'] if p.get('whole_num')}


def ensure_design_defaults(design: dict) -> dict:
    """Adds fields newer qca-core versions require but older designs lack."""
    settings = design.setdefault('simulation_settings', {})
    settings.setdefault('selected_simulation_model_id', None)
    settings.setdefault('simulation_model_settings', {})
    settings.setdefault('use_custom_input_sequence', False)
    settings.setdefault('custom_input_sequence', [])
    return design


def apply_exported_settings(design: dict, exported: dict, include_geometry: bool = False) -> dict:
    """Returns a copy of `design` that uses the exported model and its settings.

    With `include_geometry`, the exported cell architectures replace the
    design's architectures with the same id.
    """
    result = ensure_design_defaults(copy.deepcopy(design))
    model_id = exported['model']['id']
    settings = result['simulation_settings']
    settings['selected_simulation_model_id'] = model_id
    settings['simulation_model_settings'][model_id] = {
        'model_settings': copy.deepcopy(exported['model_settings']),
        'clock_generator_settings': copy.deepcopy(exported['clock_generator_settings']),
    }
    if include_geometry:
        for arch_id, architecture in exported.get('cell_architectures', {}).items():
            if arch_id in result['cell_architectures']:
                result['cell_architectures'][arch_id] = copy.deepcopy(architecture)
    return result


def _split(param_id: str) -> tuple[str, str, str | None]:
    group, _, name = param_id.partition('.')
    name, _, arg = name.partition(':')
    if not name or group not in ('geometry', 'model', 'clock'):
        raise ValueError(f'Unknown parameter {param_id!r}')
    return group, name, arg or None


def _used_architecture_ids(design: dict) -> list[str]:
    ids = []
    for layer in design['layers']:
        if layer['cell_architecture_id'] not in ids:
            ids.append(layer['cell_architecture_id'])
    return ids


def _model_settings(design: dict, group: str) -> dict:
    settings = design['simulation_settings']
    model_id = settings.get('selected_simulation_model_id')
    if model_id is None:
        raise ValueError('The design has no simulation model selected')
    model_settings = settings['simulation_model_settings'][model_id]
    return model_settings['clock_generator_settings' if group == 'clock' else 'model_settings']


def is_whole_number(design: dict, param_id: str, whole_numbers: set[str] | None = None) -> bool:
    """Whether a parameter only takes integers (listed in `whole_numbers`
    from an export, or an integer model/clock setting in the design)."""
    if whole_numbers and param_id in whole_numbers:
        return True
    group, name, _ = _split(param_id)
    if group not in ('model', 'clock'):
        return False
    value = _model_settings(design, group).get(name)
    return isinstance(value, int) and not isinstance(value, bool)


def get_parameter(design: dict, param_id: str) -> float:
    """Current value of a parameter in `design`."""
    group, name, arg = _split(param_id)
    if group in ('model', 'clock'):
        return float(_model_settings(design, group)[name])

    architecture = design['cell_architectures'][design['layers'][0]['cell_architecture_id']]
    if name == 'cell_size':
        return float(architecture['side_length'])
    if name == 'dot_radius':
        x, y = architecture['dot_positions'][0]
        return math.hypot(x, y)
    if name == 'dot_diameter':
        return float(architecture['dot_diameter'])
    if name == 'layer_z' and arg is not None:
        return float(design['layers'][int(arg)]['z_position'])
    if name in ('cell_offset_x', 'cell_offset_y') and arg is not None:
        return 0.0
    raise ValueError(f'Unknown parameter {param_id!r}')


def set_parameter(design: dict, param_id: str, value: float, whole_num: bool | None = None) -> None:
    """Sets a parameter of `design` in place.

    `whole_num` forces integer values for model/clock settings; by default
    it is inferred from the current value's type.
    """
    group, name, arg = _split(param_id)
    if group in ('model', 'clock'):
        settings = _model_settings(design, group)
        if name not in settings:
            raise ValueError(f'The model has no parameter {name!r}')
        if whole_num is None:
            whole_num = is_whole_number(design, param_id)
        settings[name] = int(round(value)) if whole_num else float(value)
        return

    if name == 'cell_size':
        nominal = get_parameter(design, param_id)
        factor = value / nominal
        for layer in design['layers']:
            for cell in layer['cells']:
                cell['position'] = [p * factor for p in cell['position']]
        for arch_id in _used_architecture_ids(design):
            design['cell_architectures'][arch_id]['side_length'] *= factor
    elif name == 'dot_radius':
        for arch_id in _used_architecture_ids(design):
            architecture = design['cell_architectures'][arch_id]
            positions = []
            for x, y in architecture['dot_positions']:
                radius = math.hypot(x, y)
                positions.append([x * value / radius, y * value / radius] if radius > 0 else [x, y])
            architecture['dot_positions'] = positions
    elif name == 'dot_diameter':
        for arch_id in _used_architecture_ids(design):
            design['cell_architectures'][arch_id]['dot_diameter'] = float(value)
    elif name == 'layer_z' and arg is not None:
        design['layers'][int(arg)]['z_position'] = float(value)
    elif name in ('cell_offset_x', 'cell_offset_y') and arg is not None:
        axis = 0 if name == 'cell_offset_x' else 1
        found = False
        for layer in design['layers']:
            for cell in layer['cells']:
                if cell.get('label') == arg:
                    cell['position'][axis] = float(cell['position'][axis] + value)
                    found = True
        if not found:
            raise ValueError(f'No cell is labelled {arg!r}')
    else:
        raise ValueError(f'Unknown parameter {param_id!r}')


def create_variant(design: dict, assignments: dict[str, float], whole_numbers: set[str] | None = None) -> dict:
    """Copy of `design` with the given parameter values.

    Cell size is applied first, so cell offsets are relative to the rescaled
    layout (the same order QCAForge uses).
    """
    result = ensure_design_defaults(copy.deepcopy(design))
    whole_numbers = whole_numbers or set()
    for param_id in sorted(assignments, key=lambda p: p != 'geometry.cell_size'):
        whole_num = True if param_id in whole_numbers else None
        set_parameter(result, param_id, assignments[param_id], whole_num)
    return result


def parse_range(text: str) -> list[float]:
    """Inclusive `start:stop:step` range, or a single value."""
    parts = [float(p) for p in text.split(':')]
    if len(parts) == 1:
        return parts
    if len(parts) != 3:
        raise ValueError(f'Expected <start:stop:step>, got {text!r}')
    start, stop, step = parts
    if start == stop:
        return [start]
    if step <= 0 or stop < start:
        raise ValueError(f'Invalid range {text!r}')
    count = math.floor((stop - start) / step + 1e-9) + 1
    return [round(start + i * step, 9) for i in range(count)]


def format_value(value: float) -> str:
    """Number formatting used in generated file names (matches QCAForge)."""
    text = f'{round(value, 9):.9f}'.rstrip('0').rstrip('.')
    return '0' if text == '-0' else text
