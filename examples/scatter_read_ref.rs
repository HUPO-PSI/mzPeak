use std::{io, path, time};

use clap::Parser;
use mzdata::prelude::*;

#[derive(Parser)]
struct App {
    #[arg()]
    filename: path::PathBuf,
}

fn main() -> io::Result<()> {
    env_logger::init();
    let args = App::parse();
    let start = time::Instant::now();

    let mut reader = mzdata::MZReader::open_path(args.filename)?;
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


    let elapsed = start.elapsed();
    eprintln!("{:0.2} seconds elapsed", elapsed.as_secs_f64());
    Ok(())
}