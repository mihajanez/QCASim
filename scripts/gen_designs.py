import argparse
import copy
import json
import math
import os
import sys
import numpy as np

from model_params import (apply_exported_settings, create_variant, ensure_design_defaults,
                          format_value, get_parameter, is_whole_number, load_parameters,
                          parse_range, whole_number_parameters)


def _set_intercell_distance(orig_design: any, original_side_length: float, side_length: float, radius: float) -> any:
    result = copy.deepcopy(orig_design)

    for layer in result['layers']:
        for cell in layer['cells']:
            cell['position'] = list(map(
                lambda p: (p/original_side_length) * side_length,
                cell['position']
            ))

    arch_id = result['layers'][0]['cell_architecture_id']
    new_pos = []
    for pos in result['cell_architectures'][arch_id]['dot_positions']:
        [x, y] = pos
        orig_radius = math.sqrt(x**2 + y**2)
        fac = radius / orig_radius
        new_pos.append([x * fac, y * fac])

    result['cell_architectures'][arch_id]['dot_positions'] = new_pos
    return result


def _set_displacement(orig_design: any, original_side_length: float, side_length: float, displacement: float) -> any:
    result = copy.deepcopy(orig_design)

    for layer in result['layers']:
        for cell in layer['cells']:
            cell['position'] = list(map(
                lambda p: (p/original_side_length) * side_length,
                cell['position']
            ))
            if cell['label'] == 'O2':
                cell['position'][1] = float(cell['position'][1] + displacement)

    return result


def generate_designs(design_filename: str, output_dir: str, spacings: list[float], radiuses: list[float]) -> int:
    count = 0
    with open(design_filename, 'r') as design_file:
        base_name = os.path.splitext(os.path.basename(design_file.name))[0]
        content = design_file.read()
        design = json.loads(content)['design']

        architectures = design['cell_architectures']
        arch_id = design['layers'][0]['cell_architecture_id']
        architecture = architectures[arch_id]

        original_side_length = architecture['side_length']

        for side_length in spacings:
            for radius in radiuses:
                new_design = ensure_design_defaults(_set_intercell_distance(
                    design, original_side_length, side_length, radius))
                with open(f'{output_dir}/{base_name}_{side_length}_{round(radius, 2)}.qcd', 'w') as new_design_file:
                    new_design_file.write(json.dumps({"design": new_design}))
                count += 1
    return count


def generate_designs_displacement(design_filename: str, output_dir: str, spacings: list[float], displacements: list[float]) -> int:
    count = 0
    with open(design_filename, 'r') as design_file:
        base_name = os.path.splitext(os.path.basename(design_file.name))[0]
        content = design_file.read()
        design = json.loads(content)['design']

        architectures = design['cell_architectures']
        arch_id = design['layers'][0]['cell_architecture_id']
        architecture = architectures[arch_id]

        original_side_length = architecture['side_length']

        for side_length in spacings:
            for displacement in displacements:
                new_design = ensure_design_defaults(_set_displacement(
                    design, original_side_length, side_length, displacement))
                with open(f'{output_dir}/{base_name}_{side_length}_{round(displacement, 2)}.qcd', 'w') as new_design_file:
                    new_design_file.write(json.dumps(
                        {"design": new_design, "designer_properties": {}}))
                count += 1

    return count


def _whole_number_values(design: dict, param_id: str, values: list[float],
                         whole_numbers: set[str]) -> list[float]:
    """Rounds (and de-duplicates) values of integer parameters, so file names
    match the value that is actually simulated."""
    if not is_whole_number(design, param_id, whole_numbers):
        return list(values)
    return list(dict.fromkeys(int(round(v)) for v in values))


def generate_parameter_sweep(design_filename: str, output_dir: str,
                             x_param: str, x_values: list[float],
                             y_param: str | None = None, y_values: list[float] | None = None,
                             params_filename: str | None = None) -> int:
    """Generates a design per (x, y) combination of any two parameters.

    Parameter ids are those of a QCAForge model parameter export (see
    model_params.py), e.g. 'geometry.cell_size', 'model.relative_permitivity'
    or 'clock.amplitude_max'. With `params_filename`, the exported model and
    settings are applied to the design first. Files are named
    <design>_<x>_<y>.qcd (y is 0 for a one-parameter sweep), which is what
    analyze_truth.py and QCAForge's robustness analysis expect.
    """
    with open(design_filename, 'r') as design_file:
        content = json.load(design_file)
    base_name = os.path.splitext(os.path.basename(design_filename))[0]
    design = ensure_design_defaults(content['design'])
    designer_properties = content.get('designer_properties', {})

    whole_numbers = set()
    if params_filename is not None:
        exported = load_parameters(params_filename)
        design = apply_exported_settings(design, exported)
        whole_numbers = whole_number_parameters(exported)

    x_values = _whole_number_values(design, x_param, x_values, whole_numbers)
    if y_param is not None:
        y_values = _whole_number_values(design, y_param, y_values, whole_numbers)

    os.makedirs(output_dir, exist_ok=True)
    count = 0
    for y in (y_values if y_param is not None else [None]):
        for x in x_values:
            assignments = {x_param: x}
            if y_param is not None:
                assignments[y_param] = y
            new_design = create_variant(design, assignments, whole_numbers)
            name = f'{base_name}_{format_value(x)}_{format_value(y or 0.0)}.qcd'
            with open(os.path.join(output_dir, name), 'w') as new_design_file:
                json.dump({'design': new_design, 'designer_properties': designer_properties},
                          new_design_file)
            count += 1
    return count


def list_parameters(design_filename: str, params_filename: str | None = None) -> None:
    """Prints the sweepable parameters of a design and their current values."""
    if params_filename is not None:
        for parameter in load_parameters(params_filename)['parameters']:
            unit = f" {parameter['unit']}" if parameter.get('unit') else ''
            print(f"{parameter['id']:<40} {parameter['value']}{unit}  ({parameter['name']})")
        return
    with open(design_filename, 'r') as design_file:
        design = ensure_design_defaults(json.load(design_file)['design'])
    ids = ['geometry.cell_size', 'geometry.dot_radius', 'geometry.dot_diameter']
    ids += [f'geometry.layer_z:{i}' for i in range(len(design['layers']))]
    labels = sorted({c['label'] for l in design['layers'] for c in l['cells'] if c.get('label')})
    ids += [f'geometry.cell_offset_{axis}:{label}' for label in labels for axis in 'xy']
    model_id = design['simulation_settings'].get('selected_simulation_model_id')
    if model_id is not None:
        settings = design['simulation_settings']['simulation_model_settings'][model_id]
        ids += [f'model.{k}' for k in settings['model_settings']]
        ids += [f'clock.{k}' for k in settings['clock_generator_settings']]
    for param_id in ids:
        print(f'{param_id:<40} {get_parameter(design, param_id)}')


def _parse_sweep(text: str) -> tuple[str, list[float]]:
    param_id, sep, values = text.partition('=')
    if not sep:
        raise argparse.ArgumentTypeError('expected <parameter>=<start:stop:step>')
    return param_id, parse_range(values)


EXAMPLES = '''examples:
  gen_designs.py line.qcd out 50:150:5 14:30:0.5
  gen_designs.py line.qcd out --x geometry.cell_size=50:150:5 --y model.relative_permitivity=10:14:0.5 --params line_parameters.json
  gen_designs.py line.qcd --list [--params line_parameters.json]'''


if __name__ == '__main__':
    parser = argparse.ArgumentParser(
        description='Generate design variants for a parameter sweep.',
        epilog=EXAMPLES, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('design', help='*.qcd design file')
    parser.add_argument('output_dir', nargs='?', help='destination folder')
    parser.add_argument('dist_range', nargs='?', help='(legacy) intercell distance range <start:stop:step>')
    parser.add_argument('radius_range', nargs='?', help='(legacy) quantum dot radius range <start:stop:step>')
    parser.add_argument('--x', type=_parse_sweep, help='first swept parameter, <parameter>=<start:stop:step>')
    parser.add_argument('--y', type=_parse_sweep, help='optional second swept parameter')
    parser.add_argument('--params', help='model parameters exported from QCAForge (*.json)')
    parser.add_argument('--list', action='store_true', help='list the sweepable parameters and exit')
    args = parser.parse_args()

    if args.list:
        list_parameters(args.design, args.params)
        sys.exit(0)
    if args.output_dir is None:
        parser.error('output_dir is required')

    if args.x is not None:
        x_param, x_values = args.x
        y_param, y_values = args.y if args.y is not None else (None, None)
        print(f'Generating {x_param} {min(x_values)} .. {max(x_values)}')
        if y_param is not None:
            print(f'Generating {y_param} {min(y_values)} .. {max(y_values)}')
        count = generate_parameter_sweep(args.design, args.output_dir, x_param, x_values,
                                         y_param, y_values, args.params)
    else:
        if args.dist_range is None or args.radius_range is None:
            parser.error('give either --x (and optionally --y) or the legacy distance and radius ranges')
        dist_range_arg = list(map(float, args.dist_range.split(':')))
        radius_range_arg = list(map(float, args.radius_range.split(':')))

        spacings = np.arange(
            dist_range_arg[0], dist_range_arg[1] + dist_range_arg[2], dist_range_arg[2])
        print(f'Generating spacing {min(spacings)} .. {max(spacings)}')

        radiuses = np.arange(
            radius_range_arg[0], radius_range_arg[1] + radius_range_arg[2], radius_range_arg[2])
        print(f'Generating radius {min(radiuses)} .. {max(radiuses)}')

        count = generate_designs(args.design, args.output_dir, spacings, radiuses)

    print(f'Generated/saved {count} designs to: {os.path.abspath(args.output_dir)}')
