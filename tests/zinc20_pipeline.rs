//! End-to-end integration test driving the public API: a ZINC20 tranche tree
//! through `LotsOfSmiles::run` into a deduplicated corpus.

use std::path::Path;

use lots_of_smiles::{Columns, Filters, LotsOfSmiles, OutputFormat, Zinc20Config};

fn write_tranche(root: &Path, sub: &str, name: &str, body: &str) {
    let dir = root.join(sub);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(name), body).unwrap();
}

#[test]
fn zinc20_end_to_end_dedups_and_filters() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("zinc");
    let scratch = dir.path().join("scratch");
    let out = dir.path().join("corpus.tsv");

    // Two tranche files. Notable rows:
    //  - ethanol appears in BOTH files (same InChIKey) -> dedup to one.
    //  - a sodium-acetate salt (multi-component) -> filtered by default.
    //  - an isotope-labelled molecule -> filtered by default.
    write_tranche(
        &root,
        "AA",
        "AAAA.txt",
        "smiles\tzinc_id\tinchikey\tmwt\tlogp\treactive\tpurchasable\ttranche_name\tfeatures\n\
         CCO\tZINC1\tLFQSCWFLJHTTHZ-UHFFFAOYSA-N\t46.07\t-0.1\t0\t50\tAAAA\t\n\
         c1ccccc1\tZINC2\tUHOVQNZJYSORNB-UHFFFAOYSA-N\t78.11\t1.9\t0\t50\tAAAA\t\n\
         CC(=O)[O-].[Na+]\tZINC3\tVMHLLURERBWHNL-UHFFFAOYSA-M\t82.03\t0.0\t0\t50\tAAAA\t\n",
    );
    write_tranche(
        &root,
        "AB",
        "ABAA.txt",
        "smiles\tzinc_id\tinchikey\tmwt\tlogp\treactive\tpurchasable\ttranche_name\tfeatures\n\
         OCC\tZINC4\tLFQSCWFLJHTTHZ-UHFFFAOYSA-N\t46.07\t-0.1\t0\t50\tABAA\t\n\
         [13CH4]\tZINC5\tVNWKTOKETHGBQD-OUBTZVSYSA-N\t17.03\t0.6\t0\t50\tABAA\t\n\
         CCN\tZINC6\tQGZKDVFQNNGYKY-UHFFFAOYSA-N\t45.08\t-0.2\t0\t50\tABAA\t\n",
    );

    let config = LotsOfSmiles::builder()
        .scratch_dir(&scratch)
        .sort_parallelism(2)
        .sort_buffer("32M")
        .zinc20(Zinc20Config::builder().root(&root).build().unwrap())
        .filters(Filters::builder().build().unwrap())
        .output(
            OutputFormat::builder()
                .path(&out)
                .columns(Columns::SmilesInchikey)
                .build()
                .unwrap(),
        )
        .build()
        .unwrap();

    let report = config.run().unwrap();

    // Six rows read; salt + isotope filtered out (2); ethanol duplicated across
    // files (1 dropped). Survivors: ethanol, benzene, ethylamine = 3 unique.
    assert_eq!(report.sources.len(), 1);
    assert_eq!(report.sources[0].stats.rows_read, 6);
    assert_eq!(report.sources[0].stats.filtered_out, 2);
    assert_eq!(report.sources[0].stats.emitted, 4);
    assert_eq!(report.dedup.unique_emitted, 3);
    assert_eq!(report.dedup.duplicates_dropped, 1);

    let content = std::fs::read_to_string(&out).unwrap();
    let lines: Vec<&str> = content.lines().collect();
    assert_eq!(lines.len(), 3);
    // Output is sorted by InChIKey; ethanol's key (LFQSC...) kept the first-seen
    // representative `CCO` from the AA tranche, not `OCC` from AB.
    assert!(content.contains("CCO\tLFQSCWFLJHTTHZ-UHFFFAOYSA-N"));
    assert!(!content.contains("OCC\t"));
    // The salt and the isotope are absent.
    assert!(!content.contains("VMHLLURERBWHNL"));
    assert!(!content.contains("VNWKTOKETHGBQD"));
}
