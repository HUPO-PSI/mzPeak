use std::{fs, io, path};

use clap::Parser;
use env_logger;
use mzpeak_prototyping::{
    MzPeakReader,
    validation,
};

#[derive(Parser)]
struct App {
    #[arg()]
    filename: path::PathBuf,
    signatures: path::PathBuf,
}

fn main() -> io::Result<()> {
    env_logger::init();
    let args = App::parse();
    let reader = MzPeakReader::new(&args.filename)?;
    let signatures_file = parquet::arrow::arrow_reader::ArrowReaderBuilder::try_new(
        fs::File::open(&args.signatures)?,
    )?
    .build()?;
    for batch in signatures_file.flatten().map(|b| validation::segment_signature_from_record_batch(&b)) {
        for segment in batch? {
            eprintln!("Verifying {segment:?}");
            let mut stream = reader.open_stream(&segment.filename)?;
            let digest = segment.read_digest_from(&mut stream)?;
            assert!(segment.verify_against(&digest).unwrap());
        }
    }

    Ok(())
}
