use std::{fs::File, path::PathBuf, sync::Arc};

use arrow::{
    array::{BooleanArray, LargeBinaryArray, RecordBatch},
    datatypes::{DataType, Field, Schema},
};
use cms::{
    cert::x509::der::{Decode, Encode, asn1::Any},
    content_info::ContentInfo,
    signed_data::SignedData,
};
use mzpeak_prototyping::validation::{
    SegmentSignature, segment_signature_from_record_batch, segment_signature_to_record_batch,
};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use sha2::Digest;

fn fixture_directory() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("test/data/provenance/self-signed-demo")
}

fn fixture_records() -> Vec<SegmentSignature> {
    let reader = ParquetRecordBatchReaderBuilder::try_new(
        File::open(fixture_directory().join("signatures.parquet")).unwrap(),
    )
    .unwrap()
    .build()
    .unwrap();
    reader
        .flat_map(|batch| segment_signature_from_record_batch(&batch.unwrap()).unwrap())
        .collect()
}

fn with_payload(batch: &RecordBatch, payload: &[u8]) -> RecordBatch {
    let mut arrays = batch.columns().to_vec();
    arrays[batch.schema().index_of("signature").unwrap()] =
        Arc::new(LargeBinaryArray::from_iter_values([payload]));
    RecordBatch::try_new(batch.schema(), arrays).unwrap()
}

#[test]
fn completed_signatures_roundtrip_without_stored_state() {
    let records = fixture_records();
    let batch = segment_signature_to_record_batch(&records).unwrap();
    assert!(batch.column_by_name("signed").is_none());
    assert_eq!(batch.schema().metadata()["mzpeak.provenance"], "1");
    assert_eq!(
        segment_signature_from_record_batch(&batch).unwrap(),
        records
    );
    let unmarked_schema = Arc::new(Schema::new(batch.schema().fields().clone()));
    let unmarked = RecordBatch::try_new(unmarked_schema, batch.columns().to_vec()).unwrap();
    assert_eq!(
        segment_signature_from_record_batch(&unmarked).unwrap(),
        records
    );
}

#[test]
fn unfinished_records_cannot_be_written() {
    let mut record = fixture_records().remove(0);
    record.signed = false;
    assert!(segment_signature_to_record_batch(&[record.clone()]).is_err());
    record.signature = sha2::Sha512::digest(b"abcdef").to_vec();
    assert!(segment_signature_to_record_batch(&[record.clone()]).is_err());
    assert!(segment_signature_to_record_batch(&[fixture_records().remove(0), record]).is_err());
}

#[test]
fn malformed_cms_cannot_be_read_or_written() {
    let mut record = fixture_records().remove(0);
    let batch = segment_signature_to_record_batch(&[record.clone()]).unwrap();
    let mut no_signers = ContentInfo::from_der(&record.signature).unwrap();
    let mut body = no_signers.content.decode_as::<SignedData>().unwrap();
    body.signer_infos = cms::signed_data::SignerInfos(Default::default());
    no_signers.content = Any::from_der(&body.to_der().unwrap()).unwrap();
    let mut wrong_type = ContentInfo::from_der(&record.signature).unwrap();
    wrong_type.content_type = "1.2.840.113549.1.7.1".parse().unwrap();
    let payloads = [
        vec![],
        vec![b'x'; 64],
        vec![b'x'; 65],
        vec![0x30, 0],
        hex::decode("300f06092a864886f70d010702a0023000").unwrap(),
        wrong_type.to_der().unwrap(),
        record.signature[..record.signature.len() - 1].to_vec(),
        [record.signature.as_slice(), &[0]].concat(),
        no_signers.to_der().unwrap(),
    ];
    for payload in payloads {
        let malformed = with_payload(&batch, &payload);
        assert!(segment_signature_from_record_batch(&malformed).is_err());
        let unmarked_schema = Arc::new(Schema::new(malformed.schema().fields().clone()));
        let unmarked = RecordBatch::try_new(unmarked_schema, malformed.columns().to_vec()).unwrap();
        assert!(segment_signature_from_record_batch(&unmarked).is_err());
        record.signature = payload;
        assert!(segment_signature_to_record_batch(&[record.clone()]).is_err());
    }
}

#[test]
fn stored_state_cannot_override_cms_parsing() {
    let record = fixture_records().remove(0);
    let batch = segment_signature_to_record_batch(&[record.clone()]).unwrap();
    let mut fields = batch.schema().fields().to_vec();
    fields.push(Arc::new(Field::new("signed", DataType::Boolean, false)));
    let mut arrays = batch.columns().to_vec();
    arrays.push(Arc::new(BooleanArray::from(vec![false])));
    let extra = RecordBatch::try_new(Arc::new(Schema::new(fields)), arrays).unwrap();
    assert_eq!(
        segment_signature_from_record_batch(&extra).unwrap(),
        vec![record]
    );
    let malformed = with_payload(&extra, b"not CMS");
    let mut arrays = malformed.columns().to_vec();
    *arrays.last_mut().unwrap() = Arc::new(BooleanArray::from(vec![true]));
    let forged = RecordBatch::try_new(malformed.schema(), arrays).unwrap();
    assert!(segment_signature_from_record_batch(&forged).is_err());
}

#[test]
fn unsupported_schema_revisions_are_rejected() {
    let batch = segment_signature_to_record_batch(&fixture_records()).unwrap();
    for revision in ["", "0", "2", "future"] {
        let schema = Schema::new_with_metadata(
            batch.schema().fields().clone(),
            std::collections::HashMap::from([("mzpeak.provenance".into(), revision.into())]),
        );
        let unsupported = RecordBatch::try_new(Arc::new(schema), batch.columns().to_vec()).unwrap();
        assert!(segment_signature_from_record_batch(&unsupported).is_err());
    }
}

#[test]
fn required_fields_reject_nulls() {
    let batch = segment_signature_to_record_batch(&[fixture_records().remove(0)]).unwrap();
    for column in [0, 6] {
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
    let directory = fixture_directory();
    let records = fixture_records();
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
