use clap::Parser;
use mzdata::{io::mgf::MGFWriter, prelude::*};
use mzpeak_prototyping::MzPeakReader;
use std::{
    io,
    path::PathBuf,
};

fn fetch(path: &PathBuf, index: usize) -> io::Result<()> {
    let mut reader = MzPeakReader::new(path)?;
    let mut spec = reader.get_spectrum(index).unwrap();
    if let Some(arrays) = spec.raw_arrays() {
        log::debug!("Loaded arrays:");
        for (k, v)  in arrays.iter() {
            log::debug!("\t{k:?} => {:?}", v.data_len());
        }
    }
    spec.pick_peaks(1.0).unwrap();

    let writer = io::stdout().lock();
    let mut writer = MGFWriter::new(writer);
    writer.write(&spec)?;
    drop(writer);

    let mut writer = io::stdout().lock();
    writeln!(writer, "Raw Data:")?;
    let arrays = spec.raw_arrays().unwrap();
    let mzs = arrays.mzs()?;
    let ints = arrays.intensities()?;
    for (mz, i) in mzs.iter().zip(ints.iter()) {
        writeln!(writer, "{mz}\t{i}")?;
    }
    Ok(())
}

#[derive(clap::Parser)]
struct App {
    #[arg()]
    path: PathBuf,

    #[arg()]
    index: usize,
}

fn main() -> io::Result<()> {
    env_logger::init();
    let args = App::parse();

    fetch(&args.path, args.index)
}
