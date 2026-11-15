# Pulse quantization vectors

`vectors.tsv` records exact IEEE-754 input bits and the resulting wire integer from
the protocol package's C# `Quantize` runtime. It covers midpoint ties and their
adjacent floats, bounds, clamping, signs and zero for the ranges used by Pulse.
The header pins the runtime source hash; changing it requires regenerating and
reviewing the vectors. These finite cases do not assert that every platform's
floating-point power implementation is identical.

From the repository root, with .NET 10 installed:

```sh
export DCL_ONE_ROOT="$PWD"
dotnet build rendering/bevy-explorer/source/crates/dcl_component/tests/quantization/Generate.csproj \
  --artifacts-path .agent-work/protocol/quantization-build
dotnet .agent-work/protocol/quantization-build/bin/Generate/debug/Generate.dll
```

The output is the TSV body; retain the source SHA-256 header when replacing it.
`tooling/tests/test_bevy_quantization.py` compiles and executes the exact Rust
arithmetic source copied into the generated Pulse module. The Bevy Explorer
`Integration guards` job runs it through `test_bevy_*.py`, including on changes
to the C# runtime under the workflow's `library/**` filter. Missing `rustc` fails
the check. The same Rust tests also run in the generated module's crate tests.
