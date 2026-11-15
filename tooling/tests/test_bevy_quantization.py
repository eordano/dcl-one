"""Compile the production Pulse arithmetic and check its exact wire vectors."""
import hashlib
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
COMPONENT = ROOT / 'rendering/bevy-explorer/source/crates/dcl_component'
REFERENCE = ROOT / 'library/protocol/protoc-gen-bitwise/runtime/cs/Quantize.cs'


class PulseQuantizationTests(unittest.TestCase):
    def test_reference_vectors_name_the_current_codec(self):
        header = (COMPONENT / 'tests/quantization/vectors.tsv').read_text().splitlines()[0]
        self.assertEqual(header, '# Quantize.cs sha256 ' + hashlib.sha256(REFERENCE.read_bytes()).hexdigest())

    def test_production_arithmetic_matches_reference_wire_values(self):
        compiler = shutil.which(os.environ.get('RUSTC', 'rustc'))
        self.assertIsNotNone(compiler, 'rustc is required for the Pulse wire regression')
        with tempfile.TemporaryDirectory() as directory:
            binary = Path(directory) / ('quantization.exe' if os.name == 'nt' else 'quantization')
            env = {**os.environ, 'DCL_ONE_ROOT': str(ROOT)}
            subprocess.run([compiler, '--edition=2021', '--test',
                            str(COMPONENT / 'src/pulse_quantization.rs'), '-o', str(binary)],
                           env=env, check=True, capture_output=True, text=True)
            subprocess.run([str(binary)], env=env, check=True, capture_output=True, text=True)


if __name__ == '__main__':
    unittest.main()
