use std::{io, path::PathBuf};

use clap::Parser;
use itertools::Itertools;
use mzdata::{self, io::tdf::TDFSpectrumReader, prelude::*};
use mzpeak_prototyping::{
    filter::median,
    grid::{GridModelLike, TimsTofMzGrid2},
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
    let mut ref_reader = TDFSpectrumReader::open_path(&args.ref_filename)?;

    let mut out = io::stdout().lock();
    writeln!(
        out,
        "id,index,is_profile,low,high,n,mean_error,max_error,median_error,model,im_model"
    )?;
    let n = ref_reader.len();
    for i in 0..n {
        let s = ref_reader.get_spectrum_by_index(i).unwrap();
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
        let low = window.lower_bound;
        let high = window.upper_bound;

        let (mz_model, im_model) = ref_reader.calibration_models_for(i);

        let grid = match mz_model {
            mzdata::io::tdf::MzCalibrationModel::Basic(_) => todo!(),
            mzdata::io::tdf::MzCalibrationModel::Model1(mz_calibration_model2) => TimsTofMzGrid2::from(mz_calibration_model2),
            mzdata::io::tdf::MzCalibrationModel::Model2(mz_calibration_model2) => TimsTofMzGrid2::from(mz_calibration_model2),
        };

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
            "{},{},{},{low},{high},{n},{mean_e},{max_e},{median_e},{:?},{:?}",
            s.id(),
            s.index(),
            s.signal_continuity().is_profile(),
            mz_model.as_param().unwrap().value().as_slice().iter().map(|v| v.to_f64().unwrap().to_string()).collect_vec().join(";"),
            im_model.as_param().unwrap().value().as_slice().iter().map(|v| v.to_f64().unwrap().to_string()).collect_vec().join(";"),
        )?;
    }

    Ok(())
}
