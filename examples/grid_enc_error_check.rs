use std::{io, path::PathBuf};

use clap::Parser;
use mzdata::{self, io::MZReader, mzsignal::PeakPicker, prelude::*, spectrum::BinaryArrayMap};
use mzpeak_prototyping::{
    filter::median, grid::{
        GridEncoding, GridModelLike, LinearGrid, SquareRootLinearGrid},
};

#[derive(Parser, Default)]
struct App {
    #[arg()]
    ref_filename: PathBuf,
    #[arg(short, long, default_value_t = 1.0)]
    scale: f64,
    #[arg(short, long)]
    ppm_error: bool,
    #[arg(short, long)]
    quadratic: bool,
}


fn numpress_peaks(
    raw_arrays: &BinaryArrayMap
) -> Vec<mzdata::mzsignal::FittedPeak> {
    let mut raw_mzs = raw_arrays
        .get(&mzdata::spectrum::ArrayType::MZArray)
        .unwrap()
        .clone();
    raw_mzs
        .store_compressed(mzdata::spectrum::bindata::BinaryCompressionType::NumpressLinear)
        .unwrap();
    let raw_mzs = raw_mzs.to_f64().unwrap();
    let intensities = raw_arrays.intensities().unwrap();
    let picker = PeakPicker::default();
    let mut peaks = Vec::new();
    picker
        .discover_peaks(&raw_mzs, &intensities, &mut peaks)
        .unwrap();
    peaks
}


fn main() -> io::Result<()> {
    env_logger::init();
    let args = App::parse();
    let ref_reader = MZReader::open_path(&args.ref_filename)?;

    let mut out = io::stdout().lock();
    writeln!(
        out,
        "id,index,is_profile,low,high,n,mean_error,max_error,median_error,nb_peaks,max_peak_error,max_numpress_peak_error"
    )?;
    let n = ref_reader.len();
    let peak_picker = PeakPicker::default();
    for s in ref_reader {
        if s.index().is_multiple_of(1000) {
            log::info!("{}/{n} ({:0.2}%)", s.index(), s.index() as f64 / n as f64 * 100.0);
        }

        let mzs: Vec<f64> = s.peaks().iter().map(|v| v.mz).collect();
        let intensities: Vec<f32> = s.peaks().iter().map(|v| v.intensity).collect();

        let low = mzs.first().copied().unwrap_or_default();
        let high = mzs.last().copied().unwrap_or_default();
        let grid: GridEncoding = if args.quadratic {
            let grid = SquareRootLinearGrid::fit(
                &mzs,
                low - 5.0,
                high + 5.0,
                args.scale,
            ).unwrap();
            grid.into()
        } else {
            let grid = LinearGrid::fit(
                &mzs,
                low - 5.0,
                high + 5.0,
                args.scale,
            ).unwrap();
            grid.into()
        };

        let e: Vec<_> = grid.error(&mzs, args.ppm_error).iter().map(|v| v.abs()).collect();
        let median_e = median(&e).unwrap_or_default();
        let (total_e, max_e) = e
            .iter()
            .copied()
            .fold((0.0, f64::NEG_INFINITY), |(total, max), ei| {
                (total + ei, max.max(ei))
            });

        let (max_peak_e, numpress_max_e, nb_peaks) = if s.signal_continuity().is_profile() {
            let mzs_hat: Vec<f64> = mzs.iter().copied().map(|m| grid.from_index(grid.to_index(m))).collect();
            let mut acc = Vec::new();
            peak_picker.discover_peaks(&mzs, &intensities, &mut acc).unwrap();
            let mut acc_hat = Vec::new();
            peak_picker.discover_peaks(&mzs_hat, &intensities, &mut acc_hat).unwrap();
            let pressed_peaks = numpress_peaks(s.raw_arrays().unwrap());
            let max_peak_e = acc.iter().zip(acc_hat.iter()).map(|(a, b)| a.mz - b.mz).reduce(|a, b| a.max(b)).unwrap_or_default();
            let numpress_max_e = acc.iter().zip(pressed_peaks.iter()).map(|(a, b)| a.mz - b.mz).reduce(|a, b| a.max(b)).unwrap_or_default();
            (max_peak_e, numpress_max_e, acc.len())
        }
        else {
            (max_e, f64::NAN, 0)
        };

        let n = mzs.len();
        let mean_e = total_e / n as f64;
        writeln!(
            out,
            "{},{},{},{low},{high},{n},{mean_e},{max_e},{median_e},{nb_peaks},{max_peak_e},{numpress_max_e}",
            s.id(),
            s.index(),
            s.signal_continuity().is_profile()
        )?;
    }

    Ok(())
}
