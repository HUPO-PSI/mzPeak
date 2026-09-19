use std::{io, path::PathBuf};

use clap::Parser;
use mzdata::{self, io::MZReader, prelude::*};
use mzpeak_prototyping::{
    filter::median,
    grid::{GridModelLike, SquareRootLinearGrid},
};

#[derive(Parser, Default)]
struct App {
    #[arg()]
    ref_filename: PathBuf,
    #[arg(short, long, default_value_t = 1.0)]
    scale: f64,
    #[arg(short, long)]
    ppm_error: bool,
}

fn main() -> io::Result<()> {
    env_logger::init();
    let args = App::parse();
    let ref_reader = MZReader::open_path(&args.ref_filename)?;

    let mut out = io::stdout().lock();
    writeln!(
        out,
        "id,index,is_profile,low,high,n,mean_error,max_error,median_error"
    )?;
    let n = ref_reader.len();
    for s in ref_reader {
        if s.index().is_multiple_of(1000) {
            log::info!("{}/{n} ({:0.2}%)", s.index(), s.index() as f64 / n as f64 * 100.0);
        }
        let mzs: Vec<f64> = s.peaks().iter().map(|v| v.mz).collect();
        let window = s
            .acquisition()
            .first_scan()
            .unwrap()
            .scan_windows
            .first()
            .unwrap();

        let low = window.lower_bound as f64;
        let high = window.upper_bound as f64;
        let grid = SquareRootLinearGrid::fit(
            &mzs,
            low - 5.0,
            high + 5.0,
            args.scale,
        ).unwrap();

        let e: Vec<_> = grid.error(&mzs, args.ppm_error).iter().map(|v| v.abs()).collect();
        let median_e = median(&e).unwrap_or_default();
        let (total_e, max_e) = e
            .iter()
            .copied()
            .fold((0.0, f64::NEG_INFINITY), |(total, max), ei| {
                (total + ei, max.max(ei))
            });
        let n = mzs.len();
        let mean_e = total_e / n as f64;
        writeln!(
            out,
            "{},{},{},{low},{high},{n},{mean_e},{max_e},{median_e}",
            s.id(),
            s.index(),
            s.signal_continuity().is_profile()
        )?;
    }

    Ok(())
}
