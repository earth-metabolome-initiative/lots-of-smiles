//! End-to-end integration test for the Enamine adapter over a pre-downloaded
//! bzip2 fixture (no network or credentials), driving `LotsOfSmiles::run`.

use std::io::Write;
use std::path::Path;

use bzip2::Compression;
use bzip2::write::BzEncoder;
use lots_of_smiles::{Columns, EnamineConfig, Filters, LotsOfSmiles, OutputFormat};

fn write_bz2(path: &Path, body: &str) {
    let file = std::fs::File::create(path).unwrap();
    let mut enc = BzEncoder::new(file, Compression::default());
    enc.write_all(body.as_bytes()).unwrap();
    enc.finish().unwrap();
}

#[test]
fn enamine_end_to_end_over_local_bz2() {
    let dir = tempfile::tempdir().unwrap();
    let download_dir = dir.path().join("enamine");
    std::fs::create_dir_all(&download_dir).unwrap();
    let scratch = dir.path().join("scratch");
    let out = dir.path().join("corpus.tsv");

    // Pre-place the 11-21 HAC bucket file so no download happens. We restrict
    // the run to that bucket via max_atoms so the other eight are skipped.
    let name = "Enamine_REAL_HAC_11_21.cxsmiles.bz2";
    write_bz2(
        &download_dir.join(name),
        "smiles\tidnumber\tType\tInChiKey\n\
         CCO\tEN1\tnormal\tLFQSCWFLJHTTHZ-UHFFFAOYSA-N\n\
         c1ccccc1 |coordinates|\tEN2\tnormal\tUHOVQNZJYSORNB-UHFFFAOYSA-N\n\
         CC(=O)[O-].[Na+]\tEN3\tnormal\tVMHLLURERBWHNL-UHFFFAOYSA-M\n",
    );

    let config = LotsOfSmiles::builder()
        .scratch_dir(&scratch)
        .sort_parallelism(2)
        .sort_buffer("32M")
        .enamine(
            EnamineConfig::builder()
                .download_dir(&download_dir)
                .build()
                .unwrap(),
        )
        .filters(Filters::builder().max_atoms(21).build().unwrap())
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

    assert_eq!(report.sources.len(), 1);
    assert_eq!(report.sources[0].tag, "enamine");
    assert_eq!(report.sources[0].stats.rows_read, 3);
    // The sodium-acetate salt is filtered (multi-component); 2 survive.
    assert_eq!(report.dedup.unique_emitted, 2);

    let content = std::fs::read_to_string(&out).unwrap();
    // CXSMILES extension stripped; salt absent.
    assert!(content.contains("CCO\tLFQSCWFLJHTTHZ-UHFFFAOYSA-N"));
    assert!(content.contains("c1ccccc1\tUHOVQNZJYSORNB-UHFFFAOYSA-N"));
    assert!(!content.contains("VMHLLURERBWHNL"));
}
