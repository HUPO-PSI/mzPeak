use std::{io, path, time};

use clap::Parser;
use mzdata::prelude::*;

#[derive(Parser)]
struct App {
    #[arg()]
    filename: path::PathBuf,

    /// Read in descending mass order
    #[arg(short, long)]
    pub by_mass: bool,
}


fn load_by_neutral_mass(
    mut reader: mzdata::MZReader<std::fs::File>
) -> io::Result<()> {

    reader.set_detail_level(mzdata::io::DetailLevel::MetadataOnly);
    let mut ions: Vec<_> = reader
        .iter()
        .filter_map(|r| {
            if r.ms_level() > 1 {
                let ion = r.precursor().unwrap().ion().unwrap();
                Some((r.index(), ion.neutral_mass(), ion.charge))
            } else {
                None
            }
        })
        .collect();
    ions.sort_by(|a, b| b.1.total_cmp(&a.1).then(b.2.cmp(&a.2)).then(b.0.cmp(&a.0)));
    reader.set_detail_level(mzdata::io::DetailLevel::Full);
    for (j, (i, m, z)) in ions.into_iter().enumerate() {
        if j % 5000 == 0 {
            log::info!("Reading {j} ({m:0.3} {z:?})");
        }
        let s = reader.get_spectrum_by_index(i).unwrap();
        assert_eq!(s.index(), i);
    }
    Ok(())
}


fn main() -> io::Result<()> {
    env_logger::init();
    let args = App::parse();
    let start = time::Instant::now();

    let mut reader = mzdata::MZReader::open_path(args.filename)?;
    if args.by_mass {
        load_by_neutral_mass(reader)?;
    } else {
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
    }


    let elapsed = start.elapsed();
    eprintln!("{:0.2} seconds elapsed", elapsed.as_secs_f64());
    Ok(())
}