use std::{collections::HashMap, fs, io, path, time};

use clap::Parser;
use env_logger;
use mzdata::{io::SpectrumSource, prelude::*};
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

    /// Read in descending mass order
    #[arg(short, long)]
    pub by_mass: bool,

    #[arg(short, long, default_value_t=10000)]
    pub chunk_size: usize,
}

fn scattered_read_from_archive(
    archive: ArchiveReader<DispatchArchiveSource>,
    filename: path::PathBuf,
) -> io::Result<()> {
    let mut reader = MzPeakReader::from_archive_reader(archive, Some(filename))?;
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

fn load_by_neutral_mass(
    archive: ArchiveReader<DispatchArchiveSource>,
    filename: path::PathBuf,
    chunk_size: usize,
) -> io::Result<()> {
    let mut reader = MzPeakReader::from_archive_reader(archive, Some(filename))?;
    let records = reader.load_all_spectrum_metadata()?.unwrap();
    let mut ions: Vec<_> = records
        .iter()
        .filter_map(|r| {
            if r.ms_level > 1 {
                let ion = r.precursor.first().unwrap().ion().unwrap();
                Some((r.index, ion.neutral_mass(), ion.charge))
            } else {
                None
            }
        })
        .collect();
    ions.sort_by(|a, b| b.1.total_cmp(&a.1).then(b.2.cmp(&a.2)).then(b.0.cmp(&a.0)));
    reader.set_spectrum_row_group_cache_size(10);
    let n = ions.len();
    let mut k = 0;
    for (j, chunk) in ions.chunks(chunk_size).enumerate() {
        log::info!("Reading {j} {:?} {k}/{n} ({:0.2}%)", chunk[0], k as f64 / n as f64 * 100.0);
        let indices = chunk.iter().map(|i| i.0);
        let spectra = reader.get_spectra_batch(indices).unwrap();
        for (i, s) in chunk.iter().zip(spectra) {
            assert_eq!(s.index(), i.0);
        }
        k += chunk.len();
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
        if args.by_mass {
            load_by_neutral_mass(archive, args.filename, args.chunk_size)?;
        } else {
            scattered_read_from_archive(archive, args.filename)?;
        }
    } else {
        let archive = ArchiveReader::<DispatchArchiveSource>::from_path_with_decryption(
            args.filename.clone(),
            dec_props,
        )?;
        if args.by_mass {
            load_by_neutral_mass(archive, args.filename, args.chunk_size)?;
        } else {
            scattered_read_from_archive(archive, args.filename)?;
        }
    }

    let elapsed = start.elapsed();
    eprintln!("{:0.2} seconds elapsed", elapsed.as_secs_f64());
    Ok(())
}
