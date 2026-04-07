use std::{collections::HashMap, fs, io, path, time};

use clap::Parser;
use env_logger;
use mzdata::{io::SpectrumSource, prelude::SpectrumLike};
use mzpeak_prototyping::{
    MzPeakReader,
    archive::{ArchiveReader, DispatchArchiveSource},
};

#[derive(Parser)]
struct App {
    #[arg()]
    filename: path::PathBuf,
    /// A secret key to use to AES decrypt the spectrum data.
    ///
    /// The key must be 16, 24, or 32 bytes long.
    #[arg(long)]
    pub encryption_key: Option<String>,

    /// Use a memory mapped file to make reads more efficient
    #[arg(short, long)]
    pub use_memmap: bool,
}

fn scattered_read_from_archive(
    archive: ArchiveReader<DispatchArchiveSource>,
    filename: path::PathBuf,
) -> io::Result<()> {
    let mut reader = MzPeakReader::from_archive_reader(archive, filename)?;
    reader.load_all_spectrum_metadata()?;
    let n = reader.len();

    let mut s;
    for i in 0..(n / 2) {
        if i % 1000 == 0 {
            log::info!("Reading {i}");
        }
        s = reader.get_spectrum_by_index(i).unwrap();
        assert_eq!(s.index(), i);
        s = reader.get_spectrum_by_index(n - (i + 1)).unwrap();
        assert_eq!(s.index(), n - (i + 1));
    }
    Ok(())
}

fn main() -> io::Result<()> {
    env_logger::init();
    let args = App::parse();
    let start = time::Instant::now();
    let mut dec_props = HashMap::default();
    if let Some(key) = args.encryption_key.as_ref() {
        dec_props
            .extend(ArchiveReader::<DispatchArchiveSource>::make_common_decryption_properties(key));
    }

    if args.use_memmap {
        // Makes this up to 2-3x faster
        let archive = unsafe {
            ArchiveReader::<DispatchArchiveSource>::memmap_with_decryption(
                fs::File::open(&args.filename)?,
                dec_props,
            )?
        };
        scattered_read_from_archive(archive, args.filename)?;
    } else {
        let archive = ArchiveReader::<DispatchArchiveSource>::from_path_with_decryption(
            args.filename.clone(),
            dec_props,
        )?;
        scattered_read_from_archive(archive, args.filename)?;
    }

    let elapsed = start.elapsed();
    eprintln!("{:0.2} seconds elapsed", elapsed.as_secs_f64());
    Ok(())
}
