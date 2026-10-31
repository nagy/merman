# merman-layout-elk

`merman-layout-elk` adapts Merman graph models to an ELK layout engine.

> **Fork notice:** this crate replaces the source-backed provider with [`elk-layout`](https://crates.io/crates/elk-layout), a pure-Rust ELK port that ships `EPL-2.0 OR GPL-3.0-or-later`. The upstream crate drove `merman-elk-layered`, which is EPL-2.0 alone and does not combine with GPL-family copyleft. Taking the GPL arm of `elk-layout` makes a GPL-3.0 or AGPL-3.0 combination lawful under GPL-3.0 section 13.

> **Implementation dependency:** applications should enable `layout-elk` on the [`merman`](https://crates.io/crates/merman) facade. This crate is the internal adapter and does not promise the complete Eclipse ELK API.

> **Distribution boundary:** this adapter depends on `elk-layout`, which contains the ported Eclipse ELK implementation under `EPL-2.0 OR GPL-3.0-or-later`. Any binary or bundled artifact that links this adapter must ship the EPL license text, attribution/provenance, and the applicable source-availability information. The adapter's own Cargo `MIT OR Apache-2.0` declaration does not describe that dependency closure.

## How it works

The transport is ELK JSON, the document format `elkjs` reads: `graph_to_json` writes the document, `elk_layout::layout_value` lays it out, and `json_to_layout_result` reads the geometry back as absolute coordinates. ELK answers a child in its container's system and hangs an edge route off the node named by the edge's `container` field, so the walk adds the ancestor origins. ELK places a label only when its text holds a character, so the adapter sends one space and keeps the label box that Merman measured itself.

## Hierarchy depth

`elk-layout` walks a nested hierarchy recursively. This crate builds the request and reads the answer iteratively, but it cannot change the port's own walk, so it rejects container nesting deeper than `MAX_HIERARCHY_DEPTH` (64) with `Error::TooDeep`. A deeper document would otherwise exhaust the caller stack instead of returning an error.

## Work budget

ELK lays a graph out in one call and offers no interruption hook, so `WorkControl` checks the budget before and after that call. A runaway layout therefore stops after it finishes, never inside it.

## Random seed authority

ELK treats `randomSeed = 0` as an unseeded request. A headless layout must not turn that into ambient process randomness, and this crate resolves the sentinel from the operation seed or from `1`, never from the process.

An operation owner that intentionally accepts the sentinel must call `layout_with_operation_seed` with an `ElkOperationSeed` created from the immutable, nonzero seed captured for that operation. This keeps replayed layouts byte-stable while preserving the configured `randomSeed` in the source model.

The workspace license is MIT OR Apache-2.0; the linked ELK port stays EPL-2.0 in its own crates.
