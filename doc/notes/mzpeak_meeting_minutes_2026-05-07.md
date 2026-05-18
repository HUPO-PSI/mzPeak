# mzPeak Session -- Meeting Minutes

**HUPO-PSI Meeting 2026, Rome**
**Date:** 7 May 2026, 07:00--12:30 (approx. 3.5 h including break)
**Location:** Aula Marconi
**Presenters:** Samuel Wein (session chair), Joshua Klein (lead developer)
**Slides:** mzPeak Discussion Guide 2026 (Joshua Klein)
**Reference:** Van Den Bossche, T. et al. 2025. *Journal of Proteome Research*. DOI: 10.1021/acs.jproteome.5c00435


## 1. Overview and Motivation

mzPeak is a next-generation open format intended to replace mzML for storing spectra, chromatograms, and instrument metadata from MS experiments. It addresses several limitations of mzML:

- mzML files are large due to XML + base64-encoded binary. Compression breaks random access.
- mzML cannot easily store imaging MS data with spatial coordinates (imzML exists but has integration problems).
- mzML cannot capture all vendor metadata.
- mzML cannot store profile and centroid data for the same spectrum simultaneously.
- No encryption support in mzML.

mzPeak uses Apache Parquet as its storage layer, which provides columnar storage, efficient compression with random access, and modular encryption. Archives are either a directory of Parquet files, an uncompressed ZIP, or a remote URL (HTTP with range requests, S3, FTP, WebDAV).


## 2. Reference Implementations

Joshua presented five independent, from-scratch implementations (none are bindings to Rust):

- **Rust** -- read/write, reference implementation (github.com/HUPO-PSI/mzPeak)
- **Python** -- read only, zero-copy Arrow/Pandas API
- **R** -- read only, dplyr-compatible API via S6 classes (contact with R for Mass Spectrometry group ongoing; S4 interface not yet implemented)
- **C#** -- read/write (github.com/HUPO-PSI/mzPeak.NET), includes demo with Thermo RawFileReader
- **JavaScript/TypeScript** -- read only, runs in browser/Node.js/Deno (online demo viewer at hupo-psi.github.io/mzpeakts/)


## 3. Format Architecture

An mzPeak archive (`.mzpeak`) contains:

- `mzpeak_index.json` -- file index describing all contents
- `spectra_metadata.parquet` / `spectra_data.parquet` -- spectrum metadata and signal data in separate streams
- `spectra_peaks.parquet` -- optional separate storage for centroid data alongside profile data
- `chromatograms_metadata.parquet` / `chromatograms_data.parquet` -- chromatogram metadata and signal
- Wavelength spectra stored independently from mass spectra
- Any additional custom/proprietary files

Signal data supports two layouts:

- **Point layout** -- one row per data point; fast to search and slice, less compressible.
- **Chunk layout** -- dense, delta-encoded, byte-shuffled, then Zstandard compressed; better compression but requires decoding before use. Chunk bounds enable granular filtering.

CV params are encoded either as Parquet columns with inflected names (`${CV}_${ACCESSION}_${CLEAN_NAME}`) or in a parameters list column. JSON metadata in Parquet footers mirrors mzML's header structure.


## 4. Benchmarks Presented

### Compression (file size)

mzPeak files are consistently smaller than mzML, often by 50% or more. Examples discussed:

- TimsTOF TDF: 1.7 GB raw, 7 GB mzML, 4 GB mzPeak
- Larger TimsTOF TDF: 5 GB raw, 23 GB mzML, 10--11 GB mzPeak
- Thermo Astral: mzPeak smaller than both raw and mzML (raw is larger because mzML discards peak metadata that mzPeak retains)
- Waters: mzPeak smaller than raw by more than half
- Agilent and SciX: mzPeak less than half of mzML but not yet matching vendor raw files due to missing grid encoding

The working hypothesis is that vendor-side implementations could beat even their own raw file sizes because vendors have access to internal grid encoding strategies.

### XIC extraction speed

mzPeak is 2--4x faster than mzML for extracted ion chromatogram queries across TimsTOF, Astral, Agilent, and Waters data. Parquet page indices enable efficient sparse reads. Batching reads (e.g., 5000--15000 spectra) and memory mapping further improve performance.


## 5. Discussion Topics and Decisions

### 5.1 Profile vs. centroid data storage

**Question:** When a file contains both profile and centroid spectra, should centroid-only spectra go in `_data` or `_peaks`?

**Decision:** Centroid data will always be stored in the `_peaks` file. Profile data goes in `_data`. This was agreed by consensus. The metadata table will carry both `number of data points` (MS:1003060) and `number of peaks` (MS:1003059) columns to indicate which files to read.

**Open issue:** For TimsTOF data where spectra are centroided in m/z but profiled in ion mobility, the consensus was to place them in the centroid/peaks dimension for mass spectra, but this needs to be specified clearly in the specification.

### 5.2 Namespace'd index columns

**Question:** Should columns like `spectrum_index`, `spectrum_time` keep the namespace prefix, or be simplified to `index`, `time`?

**Discussion:** The namespace prefix prevents misinterpretation (e.g., confusing a spectrum time column with a chromatographic time array) but adds implementation overhead. Strong-typing analogy raised: prefixes protect against silent misinterpretation.

**Decision:** Keep the namespace prefixes. The performance cost is negligible; the safety benefit outweighs the ergonomic cost.

### 5.3 Consistent ID types

**Question:** Should identifiers be strings or integers?

**Discussion:** Integers are compact and avoid heap allocation. Strings are more descriptive. Currently inconsistent (instrument configurations use integers, most other entities use strings). No strong consensus; left as an open item.

### 5.4 Additional data modalities

**Question:** Are there modalities beyond mass spectra, wavelength spectra, and chromatograms that need separate storage?

**Discussed:**

- **Ion mobilograms** (ion mobility as primary axis): could be formalized as a separate facet. Not required yet.
- **Diagnostic traces** (pump pressure, source current, temperature, voltages): consensus to store these in a new namespace (working name: "diagnostic traces"), separate from chromatograms.
- **Imaging MS**: currently handled via pixel coordinates in the metadata table. ZARR was considered as an alternative but Parquet was chosen for long-term stability and cross-language support. Regions of interest can be stored as spatial annotation polygons on top. Lisa Boatner noted she uses ZARR for imaging MS in metabolomics and proteomics.
- **Intelligent data acquisition traces**: raised as a future need for traceability of instrument decision-making during acquisition. Can be encoded as CV params per scan. Samuel Wein noted that OpenMS has been working with the Thermo IAPI for intelligent data acquisition in top-down proteomics, and that ontology work is needed for cross-instrument interoperability.

### 5.5 Processed data views

**Question:** Should we formalize storage of processed data beyond peaks (e.g., feature bounding boxes, noise estimates, alternative sort indices, XIC peak shape models)?

**Discussion:** The file index overlay/append mechanism allows adding processed data to an archive. Open questions remain about uniqueness requirements and whether archives should support fragmented references to external files. No final decision; deferred to specification work.

### 5.6 Recalibration and file modification

**Discussion:** Eric Deutsch raised the use case of recalibrating m/z values across all MS2 spectra. Several approaches were outlined:

1. Rewrite the full archive (safest, simplest).
2. Append a new Parquet file to the ZIP and update the file index to redirect reads.
3. Unpack, symlink unchanged files, add new files, repack.

Trade-offs around archive corruption, size growth, and portability were discussed. No final decision on a recommended approach.

### 5.7 SDRF integration

SDRF details known at acquisition time could be stored inside the mzPeak archive, either as a TSV file or via the sample list metadata inherited from mzML. This remains an open design question.


## 6. Live Demos

Joshua demonstrated:

1. **Browser-based spectrum viewer** (TypeScript/React) reading local and remote mzPeak files, including XIC extraction, base peak chromatogram, and metadata inspection.
2. **Python API** -- reading TimsTOF data (20 GB mzML equivalent) in ~4 seconds, querying metadata as Pandas DataFrames, accessing centroid peaks with baseline/S:N/resolution arrays from Thermo data, plotting diagnostic traces (source current, flow rate). Ralf Gabriels suggested a live demo of mapping MS2 spectra IDs to filter strings, which Joshua implemented on the spot.
3. **R API** -- reading mzPeak files using S6 classes, dplyr-compatible table access.
4. **Arbitrary file storage** -- demonstrated reading vendor-specific proprietary data (Thermo sampled noise arrays) stored as an extra Parquet file in the archive.


## 7. Roadmap and Next Steps

### Specification

- Joshua will incorporate all feedback from this meeting into the specification within approximately 3 weeks.
- The specification currently lives as a Markdown file in the main GitHub repository. This location was confirmed as acceptable.
- The specification is written in a conversational style and needs to be formalized (potentially using ASCIIdoc or staying with Markdown).
- **Critical need:** external reviewers to read the spec with fresh eyes and report unclear or missing information. Zero feedback has been received so far. Issues can be filed on GitHub, or sent via email.

### OpenAPI specification

- Joshua will produce a language-agnostic OpenAPI-style specification defining the common API across all implementations (~40--50 methods: open, get_spectrum, get_chromatogram, iterate, slice, etc.).
- Estimated timeline: 2--3 months (end of summer 2026).

### Validator

- A validator will check: correct file index structure, schema consistency, JSON schema compliance, CV accession/name matching, data type reasonableness (e.g., warning if MS level is stored as int64), and cross-file consistency (e.g., number of data points in metadata vs. actual data).
- Semantic validation will follow mzML precedent.
- Validator development follows after the API specification is stable.

### PSI document process

- The specification should be submitted to the PSI document process. This will happen in parallel with paper writing, ideally once the spec is stable and key tool integrations are in place.

### Vendor outreach and tool integration

- Samuel Wein plans to pitch mzPeak to vendors at ASMS 2026 (San Diego). Goal: get firm commitments on integration with vendor ecosystems.
- Ralf Gabriels suggested targeted outreach to ~5 specific people (e.g., ProteoWizard/msconvert developers, mzML developers) for specification review.
- OpenMS is developing a C/C++ implementation that should integrate into ProteoWizard/msconvert.
- Ralf Gabriels also noted the need for a user-friendly, cross-platform, easily installable conversion UI (potentially within msconvert).

### Trademark

- OpenMS Inc. is registering the "mzPeak" trademark, with Tim Van Den Bossche helping to prepare the statement. The trademark is held in trust for the PSI committee until a suitable legal entity is established. It ensures no incompatible implementations use the mzPeak name. The standard remains open.

### Manuscript

- Paper writing should begin once the OpenAPI spec and key tool integrations (msconvert output, at least one tool reading mzPeak) are in place, likely end of summer 2026.


## Action Items

| # | Action | Owner | Timeline |
|---|--------|-------|----------|
| 1 | Incorporate meeting feedback into the mzPeak specification | Joshua Klein | ~3 weeks (end of May 2026) |
| 2 | Produce OpenAPI specification for common cross-language API | Joshua Klein | End of summer 2026 |
| 3 | Build validator (file structure, schema, semantic checks) | Joshua Klein | After API spec is stable |
| 4 | Identify and contact ~5 targeted spec reviewers (ProteoWizard, mzML devs) | Samuel Wein + Joshua Klein | Before ASMS |
| 5 | Vendor outreach at ASMS 2026 | Samuel Wein | ASMS (June 2026) |
| 6 | Complete C/C++ implementation (standalone library for ProteoWizard integration) | OpenMS team | Ongoing |
| 7 | Integrate mzPeak output into msconvert | Joshua (+ ProteoWizard team) | TBD |
| 8 | Implement S4 interface for R (R for Mass Spectrometry compatibility) | Joshua Klein | TBD |
| 9 | Investigate grid encoding for TimsTOF/Agilent/SciX to improve compression | Joshua Klein | Ongoing |
| 10 | Submit specification to PSI document process | Samuel Wein + Joshua Klein | In parallel with paper |
| 11 | Begin manuscript writing | Samuel Wein + Joshua Klein + contributors | End of summer 2026 |
| 12 | Finalize trademark registration for "mzPeak" | Tim Van Den Bossche / OpenMS Inc. | Ongoing |
| 13 | All attendees: review the specification on GitHub and provide feedback | All | ASAP |
