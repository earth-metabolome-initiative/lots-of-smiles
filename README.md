# lots-of-smiles

[![CI](https://github.com/earth-metabolome-initiative/lots-of-smiles/actions/workflows/ci.yml/badge.svg)](https://github.com/earth-metabolome-initiative/lots-of-smiles/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![Rust edition 2024](https://img.shields.io/badge/rust-edition%202024-orange.svg)](https://doc.rust-lang.org/edition-guide/rust-2024/index.html)
[![DOI](https://zenodo.org/badge/DOI/10.5281/zenodo.20799971.svg)](https://doi.org/10.5281/zenodo.20799971)
[![Hugging Face](https://img.shields.io/badge/%F0%9F%A4%97%20Hugging%20Face-dataset-yellow)](https://huggingface.co/datasets/EarthMetabolomeInitiative/lots-of-smiles)

😀😃😄😁😆😊🙂😀😃😄😁😆😊🙂😀😃😄😁😆😊🙂😀😃😄😁😆😊🙂😀😃😄😁😆😊🙂😀😃😄😁😆😊🙂😀😃😄😁😆😊🙂😀😃😄😁😆😊🙂😀😃😄😁😆😊🙂😀😃😄😁😆😊🙂

A Rust library and command-line tool that assembles one large, deduplicated, canonicalized collection of real-world SMILES from public molecular databases (PubChem, ZINC20, and the Enamine REAL™ Database). It is the reproducibility code for a published corpus of **15,260,616,134 unique canonical SMILES** (about 48 GB, zstd-compressed), available on [Zenodo](https://doi.org/10.5281/zenodo.20799971) and [Hugging Face](https://huggingface.co/datasets/EarthMetabolomeInitiative/lots-of-smiles), which document the dataset, its provenance, and licensing.

## What the tool does

The pipeline ingests each source in full (PubChem as a CID merge-join of `CID-SMILES.gz` and `CID-InChI-Key.gz`, ZINC20 as the local 2D tranche tree, Enamine REAL as HAC-bucketed `.cxsmiles.bz2` files, downloaded if absent), applies a configurable filter pass (see [Filtering](#filtering)), and stages records as `INCHIKEY <TAB> PRIORITY <TAB> SMILES`. GNU `sort` then orders them under `LC_ALL=C` and a streaming pass keeps the first line of each InChIKey run, giving deterministic first-wins dedup where earlier-configured sources win collisions. Identity comes from each source's shipped InChIKey, so no chemistry toolkit is involved. Canonicalization is a separate, optional step (`lots-of-smiles canonicalize`, pure Rust via `smiles-parser`) followed by `sort -u`, which collapses the same molecule written differently across sources.

## Sources

| Source       | Scale            | How it is obtained                                                          |
|--------------|------------------|----------------------------------------------------------------------------|
| PubChem      | ~120M molecules  | local `CID-SMILES.gz` joined with `CID-InChI-Key.gz` (downloaded from NCBI) |
| ZINC20       | ~1.93B molecules | local tranche tree `<root>/<XX>/<YYYY>.txt`                                 |
| Enamine REAL™ | ~13.6B molecules | authenticated download of HAC-bucketed `.cxsmiles.bz2` files               |

## Filtering

Per-run filters (all optional) cover size (min/max molecular mass in Da, min/max atom count), composition (element whitelist or blacklist, isotope policy, radical policy), and connectivity (single connected component, net-charge bound). Cheap byte-level checks (single component, isotopes) run before any parsing, while parser-backed checks run only when configured. For Enamine, heavy-atom bounds additionally skip whole HAC-bucketed files at download time. Defaults: single connected component required, isotopes and radicals forbidden, no size or element bounds.

## Building a corpus

```sh
lots-of-smiles \
  --source zinc20 --source pubchem --source enamine \
  --zinc20-root /path/to/zinc20/2D \
  --pubchem-cid-smiles-gz /path/to/CID-SMILES.gz \
  --pubchem-cid-inchikey-gz /path/to/CID-InChI-Key.gz \
  --enamine-dir /path/to/enamine \
  --allow-isotopes --allow-radicals \
  --columns smiles-inchikey --compression zstd \
  --scratch-dir /path/to/scratch \
  --sort-parallelism 64 --sort-buffer 200G \
  --output /path/to/corpus.smi.zst
```

A JSON run report is printed to stdout and written next to the output as `<output>.report.json`.

### Enamine credentials

Enamine REAL downloads require an Enamine.net account. Provide credentials via a `.env` file (copy [.env.example](.env.example) to `.env`, which is gitignored) or environment variables. Wrap values containing special characters in single quotes:

```sh
ENAMINE_USERNAME='you@example.org'
ENAMINE_PASSWORD='your-password'
```

`--download-only` fetches the source files without building a corpus. Credentials are only needed to download. If the `.cxsmiles.bz2` files are already present in `--enamine-dir`, no login occurs.

## Canonicalizing

The `lots-of-smiles canonicalize` subcommand is a parallel stdin-to-stdout filter that maps each SMILES to its canonical form (failures are dropped and counted, optionally written with `--failed`):

```sh
zstd -dc --long=31 corpus.smiles-only.smi.zst \
  | lots-of-smiles canonicalize --threads 60 --total 15388157262 \
  | LC_ALL=C sort -u --parallel=64 -S 200G \
  | zstd -19 --long=31 -T0 -o corpus.canonical.smi.zst
```

## Publishing to Zenodo

Depositing the corpus on Zenodo is behind the optional `zenodo` feature, which adds the `publish-zenodo` subcommand. It creates a draft with full dataset metadata (title, description, creators, license `cc-by-4.0`, keywords, related software identifiers, and the Enamine attribution), uploads the file with a progress bar, and stops at the draft so you can review it in the Zenodo UI. Pass `--publish` to publish directly. The token and depositor identity (`ZENODO_TOKEN` or `ZENODO_SANDBOX_TOKEN`, plus `ZENODO_CREATOR`, `ZENODO_ORCID`, `ZENODO_AFFILIATION`) come from the environment, typically a `.env` file (see [.env.example](.env.example)). Test on the sandbox first:

```sh
cargo run --release --features zenodo -- \
  publish-zenodo /path/to/corpus.canonical.smi.zst --sandbox
```

## Library usage

```rust,no_run
use lots_of_smiles::{LotsOfSmiles, Filters, OutputFormat, Zinc20Config, Columns};

let report = LotsOfSmiles::builder()
    .scratch_dir("/path/to/scratch")
    .zinc20(Zinc20Config::builder().root("/path/to/zinc20/2D").build()?)
    .filters(Filters::builder().max_mass_da(900.0).build()?)
    .output(OutputFormat::builder().path("/path/to/corpus.smi").columns(Columns::SmilesInchikey).build()?)
    .build()?
    .run()?;
println!("{} unique molecules", report.dedup.unique_emitted);
# Ok::<(), lots_of_smiles::LosError>(())
```

All configuration types are builders with private fields whose `build()` validates the configuration.

## Building from source

```sh
cargo build --release
cargo test
```

Requires a `sort` binary on `PATH` (GNU coreutils) and `zstd`.

## Resource requirements

Building the full corpus is a heavy, multi-day job, not a laptop task. The reference build used a 64-core workstation, 1 TB of RAM, and a roughly 15 TB NVMe volume, and took about two days plus a similar span for canonicalization. Storage is the main constraint (use NVMe): the run stages around 1 TB of intermediate TSV, needs comparable space for the external sort, and about 330 GB for the Enamine downloads. More RAM means fewer `sort` merge passes, and canonicalization is CPU-bound at roughly a hundred times the cost of parsing, so cores help. A single source, or atom-count bounds that skip most Enamine HAC buckets, brings this down to hours.

## Licensing

This repository (the code) is released under the [MIT License](LICENSE).

The **dataset** is a separate artifact with its own terms. It is released under CC BY 4.0, and the REAL-derived structures are published with Enamine's express written permission, so any use must credit the Enamine REAL™ Database and preserve the trademark notice. See the [Zenodo record](https://doi.org/10.5281/zenodo.20799971) for the full dataset license and attribution requirements. The MIT license on this code does not grant any rights over the dataset or the underlying source databases.

## Acknowledgements

This work builds on PubChem (U.S. National Library of Medicine), ZINC20 (Irwin and Shoichet group, UCSF), and the Enamine REAL™ Database (Enamine Ltd). Please cite these original sources in any derived work. Parsing and canonicalization use [`smiles-parser`](https://github.com/earth-metabolome-initiative/smiles-parser).
