use std::sync::Arc;

use arrow::{array::RecordBatch, datatypes::Schema};
use mzpeak_prototyping::validation::{
    SegmentSignature, segment_signature_from_record_batch, segment_signature_to_record_batch,
};
use sha2::Digest;

fn segment() -> SegmentSignature {
    SegmentSignature {
        filename: "metadata.json".into(),
        ..Default::default()
    }
}

#[test]
fn nullable_records_and_signature_state_roundtrip() {
    let mut unsigned = segment();
    unsigned.signature = sha2::Sha512::digest(b"abcdef").to_vec();
    let signed = SegmentSignature {
        signed: true,
        signature: vec![0; 200],
        ..unsigned.clone()
    };
    let records = vec![unsigned, signed];
    let batch = segment_signature_to_record_batch(&records);
    assert_eq!(
        segment_signature_from_record_batch(&batch).unwrap(),
        records
    );
    let legacy = batch.project(&[0, 1, 2, 3, 4, 5, 6]).unwrap();
    assert_eq!(
        segment_signature_from_record_batch(&legacy).unwrap(),
        records
    );
}

#[test]
fn required_fields_reject_nulls() {
    let batch = segment_signature_to_record_batch(&[segment()]);
    for column in [0, 6, 7] {
        let mut arrays = batch.columns().to_vec();
        arrays[column] = arrow::array::new_null_array(arrays[column].data_type(), 1);
        let schema = Schema::new(
            batch
                .schema()
                .fields()
                .iter()
                .map(|field| field.as_ref().clone().with_nullable(true))
                .collect::<Vec<_>>(),
        );
        let malformed = RecordBatch::try_new(Arc::new(schema), arrays).unwrap();
        assert!(segment_signature_from_record_batch(&malformed).is_err());
    }
}

#[cfg(feature = "verification")]
#[test]
fn python_demo_mixed_records_load_and_verify() -> Result<(), Box<dyn std::error::Error>> {
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
    use std::{fs::File, path::Path};

    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("test/data/provenance/legacy-demo");
    let batches = ParquetRecordBatchReaderBuilder::try_new(File::open(
        directory.join("signatures.parquet"),
    )?)?
    .build()?;
    let mut records = vec![];
    for batch in batches {
        let batch = batch?;
        let decoded = segment_signature_from_record_batch(&batch)?;
        let legacy = batch.project(&[0, 1, 2, 3, 4, 5, 6])?;
        assert_eq!(segment_signature_from_record_batch(&legacy)?, decoded);
        records.extend(decoded);
    }
    assert_eq!(records.len(), 3);
    assert!(records.iter().all(|record| record.signed));
    let whole = records
        .iter()
        .find(|r| r.filename == "metadata.json")
        .unwrap();
    assert_eq!(
        (whole.row_group, whole.column, whole.offset, whole.length),
        (None, None, None, None)
    );
    assert!(whole.path.is_none());
    let chunks: Vec<_> = records
        .iter()
        .filter(|r| r.filename == "spectrum.parquet")
        .collect();
    assert_eq!(chunks.len(), 2);
    assert!(chunks.iter().all(|r| r.row_group == Some(0)
        && r.column.is_some()
        && r.offset.is_some()
        && r.length.is_some()));
    for record in records {
        let digest = record.read_digest_from(&mut File::open(directory.join(&record.filename))?)?;
        assert!(record.verify_against(&digest).unwrap());
        assert!(
            !record
                .verify_against(&sha2::Sha512::digest(b"tampered"))
                .unwrap()
        );
    }
    Ok(())
}
