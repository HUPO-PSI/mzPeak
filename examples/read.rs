use std::{collections::HashMap, io, path, time};

use clap::Parser;
use env_logger;
use mzdata::prelude::SpectrumLike;
use mzpeak_prototyping::{MzPeakReader, archive::{ArchiveReader, DispatchArchiveSource, MzPeakArchiveType}};
use parquet::encryption::decrypt::FileDecryptionProperties;

#[derive(Parser)]
struct App {
    #[arg()]
    filename: path::PathBuf,
    /// A secret key to use to AES decrypt the spectrum data.
    ///
    /// The key must be 16, 24, or 32 bytes long.
    #[arg(long)]
    pub encryption_key: Option<String>,
}

fn main() -> io::Result<()> {
    env_logger::init();
    let args = App::parse();
    let start = time::Instant::now();
    let mut dec_props = HashMap::default();
    if let Some(key) = args.encryption_key.as_ref() {
        let dec = FileDecryptionProperties::builder(key.as_bytes().to_vec()).build().unwrap();
        dec_props.insert(MzPeakArchiveType::SpectrumDataArrays.tag_file_suffix().to_string(), dec.clone());
        dec_props.insert(MzPeakArchiveType::SpectrumMetadata.tag_file_suffix().to_string(), dec.clone());
    }

    let archive = ArchiveReader::<DispatchArchiveSource>::from_path_with_decryption(
        args.filename.clone(),
        dec_props
    )?;
    let reader = MzPeakReader::from_archive_reader(archive, args.filename)?;
    log::info!("Opened in {:0.2} seconds", start.elapsed().as_secs_f64());
    let mut i = 0;
    let mut points = 0;
    for spec in reader {
        log::debug!("Read spectrum {i}");
        if i % 5000 == 0 {
            log::info!("Read spectrum {i}");
        }
        i += 1;
        let arrays = spec.raw_arrays().unwrap();
        match arrays.mzs() {
            Ok(arr) => {
                points += arr.len();
                let ints = arrays.intensities().unwrap();
                assert_eq!(
                    arr.len(),
                    ints.len(),
                    "{} had {} m/z values and {} intensities, {arr:?} {ints:?}",
                    spec.index(),
                    arr.len(),
                    ints.len()
                );
            }
            Err(e) => {
                eprintln!(
                    "Failed to retrieve arrays for spectrum {}: {e}",
                    spec.index()
                );
            }
        }
    }
    let dur = start.elapsed();
    eprintln!(
        "Read {i} spectra and {points} points. Elapsed: {} seconds",
        dur.as_secs_f64()
    );
    Ok(())
}
