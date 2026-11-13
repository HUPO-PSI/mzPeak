use std::{fs, io, path};

use clap::Parser;
use mzpeak_prototyping::{MzPeakReader, validation};

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
    let mut count = 0;
    for batch in signatures_file {
        let batch = batch.map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        for segment in validation::segment_signature_from_record_batch(&batch)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?
        {
            eprintln!("Verifying {segment:?}");
            let mut stream = reader.open_stream(&segment.filename)?;
            let digest = segment.read_digest_from(&mut stream)?;
            let result = segment
                .verify_against(&digest)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            if !result.is_valid() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "segment digest does not match",
                ));
            }
            count += 1;
        }
    }

    if count == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "signature table is empty",
        ));
    }
    eprintln!(
        "Checked {count} legacy signatures. Certificate trust, segment identity, and archive coverage were not established."
    );
    Ok(())
}
