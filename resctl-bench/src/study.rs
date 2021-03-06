// Copyright (c) Facebook, Inc. and its affiliates.
#![allow(dead_code)]
use anyhow::{bail, Result};
use log::error;
use num_traits::cast::AsPrimitive;
use quantiles::ckms::CKMS;
use std::collections::BTreeMap;
use std::fmt::Write;
use util::*;

use super::run::RunCtx;
use rd_agent_intf::{IoLatReport, Report};

pub trait Study {
    fn study(&mut self, rep: &Report) -> Result<()>;
    fn as_study_mut(&mut self) -> &mut dyn Study;
}

//
// Sel helpers.
//
pub fn sel_factory_iolat(io_type: &str, pct: &str) -> impl Fn(&Report) -> Option<f64> + Clone {
    let io_type = io_type.to_string();
    let pct = pct.to_string();
    move |rep: &Report| {
        if rep.iolat.map[&io_type]["100"] > 0.0 {
            Some(rep.iolat.map[&io_type][&pct])
        } else {
            None
        }
    }
}

//
// Calculate average, min and max.
//
pub struct StudyMean<T, F>
where
    T: AsPrimitive<f64>,
    F: Fn(&Report) -> Option<T>,
{
    sel: F,
    data: Vec<f64>,
}

impl<T, F> StudyMean<T, F>
where
    T: AsPrimitive<f64>,
    F: Fn(&Report) -> Option<T>,
{
    pub fn new(sel: F) -> Self {
        Self { sel, data: vec![] }
    }
}

impl<T, F> Study for StudyMean<T, F>
where
    T: AsPrimitive<f64>,
    F: Fn(&Report) -> Option<T>,
{
    fn study(&mut self, rep: &Report) -> Result<()> {
        if let Some(v) = (self.sel)(rep) {
            self.data.push(v.as_());
        }
        Ok(())
    }

    fn as_study_mut(&mut self) -> &mut dyn Study {
        self
    }
}

pub trait StudyMeanTrait: Study {
    fn result(&self) -> (f64, f64, f64, f64);
}

impl<T, F> StudyMeanTrait for StudyMean<T, F>
where
    T: AsPrimitive<f64>,
    F: Fn(&Report) -> Option<T>,
{
    fn result(&self) -> (f64, f64, f64, f64) {
        let mean = statistical::mean(&self.data);
        let stdev = match self.data.len() {
            1 => 0.0,
            _ => statistical::standard_deviation(&self.data, None),
        };
        let mut min = std::f64::MAX;
        let mut max = std::f64::MIN;
        for v in self.data.iter() {
            min = min.min(*v);
            max = max.max(*v);
        }

        (mean, stdev, min, max)
    }
}

//
// Calculate percentiles.
//
pub struct StudyPcts<T, F>
where
    T: AsPrimitive<f64>,
    F: Fn(&Report) -> Option<T>,
{
    sel: F,
    ckms: CKMS<f64>,
}

impl<T, F> StudyPcts<T, F>
where
    T: AsPrimitive<f64>,
    F: Fn(&Report) -> Option<T>,
{
    pub fn new(sel: F, error: Option<f64>) -> Self {
        const CKMS_DFL_ERROR: f64 = 0.001;
        Self {
            sel,
            ckms: CKMS::<f64>::new(error.unwrap_or(CKMS_DFL_ERROR)),
        }
    }
}

impl<T, F> Study for StudyPcts<T, F>
where
    T: AsPrimitive<f64>,
    F: Fn(&Report) -> Option<T>,
{
    fn study(&mut self, rep: &Report) -> Result<()> {
        if let Some(v) = (self.sel)(rep) {
            self.ckms.insert(v.as_());
        }
        Ok(())
    }

    fn as_study_mut(&mut self) -> &mut dyn Study {
        self
    }
}

pub trait StudyPctsTrait: Study {
    fn result(&self, pcts: &[&str]) -> BTreeMap<String, f64>;
}

impl<T, F> StudyPctsTrait for StudyPcts<T, F>
where
    T: AsPrimitive<f64>,
    F: Fn(&Report) -> Option<T>,
{
    fn result(&self, pcts: &[&str]) -> BTreeMap<String, f64> {
        pcts.iter()
            .map(|pct| {
                let pctf = pct.parse::<f64>().unwrap() / 100.0;
                (pct.to_string(), self.ckms.query(pctf).map(|x| x.1).unwrap())
            })
            .collect()
    }
}

//
// Calculate mean and percentiles.
//
pub struct StudyMeanPcts<T, F>
where
    T: AsPrimitive<f64>,
    F: Fn(&Report) -> Option<T>,
{
    study_mean: StudyMean<T, F>,
    study_pcts: StudyPcts<T, F>,
}

impl<T, F> StudyMeanPcts<T, F>
where
    T: AsPrimitive<f64>,
    F: Fn(&Report) -> Option<T> + Clone,
{
    pub fn new(sel: F, error: Option<f64>) -> Self {
        Self {
            study_pcts: StudyPcts::<T, F>::new(sel.clone(), error),
            study_mean: StudyMean::<T, F>::new(sel),
        }
    }
}

impl<T, F> Study for StudyMeanPcts<T, F>
where
    T: AsPrimitive<f64>,
    F: Fn(&Report) -> Option<T> + Clone,
{
    fn study(&mut self, rep: &Report) -> Result<()> {
        self.study_mean.study(rep).and(self.study_pcts.study(rep))
    }

    fn as_study_mut(&mut self) -> &mut dyn Study {
        self
    }
}

pub trait StudyMeanPctsTrait: Study {
    fn result(&self, pcts: &[&str]) -> (f64, f64, BTreeMap<String, f64>);
}

impl<T, F> StudyMeanPctsTrait for StudyMeanPcts<T, F>
where
    T: AsPrimitive<f64>,
    F: Fn(&Report) -> Option<T> + Clone,
{
    fn result(&self, pcts: &[&str]) -> (f64, f64, BTreeMap<String, f64>) {
        let (mean, stdev, _, _) = self.study_mean.result();
        let pcts = self.study_pcts.result(pcts);
        (mean, stdev, pcts)
    }
}

//
// Helpers.
//
pub struct StudyIoLatPcts {
    io_type: String,
    studies: Vec<Box<dyn StudyMeanPctsTrait>>,
}

impl StudyIoLatPcts {
    pub const LAT_PCTS: &'static [&'static str] = &IoLatReport::PCTS;
    pub const TIME_PCTS: [&'static str; 14] = [
        "00", "01", "05", "10", "16", "25", "50", "75", "84", "90", "95", "99", "99.9", "100",
    ];
    pub const TIME_FORMAT_PCTS: [&'static str; 9] =
        ["00", "16", "50", "84", "90", "95", "99", "99.9", "100"];

    pub fn new(io_type: &str, error: Option<f64>) -> Self {
        Self {
            io_type: io_type.to_string(),
            studies: Self::LAT_PCTS
                .iter()
                .map(|pct| {
                    Box::new(StudyMeanPcts::new(sel_factory_iolat(io_type, pct), error))
                        as Box<dyn StudyMeanPctsTrait>
                })
                .collect(),
        }
    }

    pub fn studies(&mut self) -> Vec<&mut dyn Study> {
        self.studies
            .iter_mut()
            .map(|study| study.as_study_mut())
            .collect()
    }

    pub fn result(
        &self,
        rctx: &RunCtx,
        time_pcts: Option<&[&str]>,
    ) -> BTreeMap<String, BTreeMap<String, f64>> {
        let mut result = BTreeMap::<String, BTreeMap<String, f64>>::new();
        for (lat_pct, study) in Self::LAT_PCTS.iter().zip(self.studies.iter()) {
            let (mean, stdev, mut pcts) = study.result(&time_pcts.unwrap_or(&Self::TIME_PCTS));
            pcts.insert("mean".to_string(), mean);
            pcts.insert("stdev".to_string(), stdev);
            result.insert(lat_pct.to_string(), pcts);
        }

        rctx.access_agent_files(|af| {
            for lat_pct in Self::LAT_PCTS.iter() {
                result.get_mut(*lat_pct).unwrap().insert(
                    "cum".to_string(),
                    af.report.data.iolat_cum.map[&self.io_type][*lat_pct],
                );
            }
        });
        result
    }

    pub fn format_table<'a>(
        out: &mut Box<dyn Write + 'a>,
        result: &BTreeMap<String, BTreeMap<String, f64>>,
        time_pcts: Option<&[&str]>,
    ) {
        let time_pcts = time_pcts
            .unwrap_or(&Self::TIME_FORMAT_PCTS)
            .iter()
            .chain(Some("cum").iter())
            .chain(Some("mean").iter())
            .chain(Some("stdev").iter());
        write!(out, "       ").unwrap();

        let widths: Vec<usize> = time_pcts
            .clone()
            .map(|pct| (pct.len() + 1).max(5))
            .collect();

        let fmt_pct = |pct: &str| -> String {
            match pct {
                "cum" | "mean" | "stdev" => pct.to_string(),
                pct => {
                    let pctf = pct.parse::<f64>().unwrap();
                    if pctf == 0.0 {
                        "min".to_string()
                    } else if pctf == 100.0 {
                        "max".to_string()
                    } else {
                        format!("p{}", pct)
                    }
                }
            }
        };

        for (pct, width) in time_pcts.clone().zip(widths.iter()) {
            write!(out, " {:>1$}", &fmt_pct(*pct), width).unwrap();
        }

        for lat_pct in Self::LAT_PCTS.iter() {
            write!(out, "\n{:<7}", &fmt_pct(*lat_pct)).unwrap();
            for (time_pct, width) in time_pcts.clone().zip(widths.iter()) {
                write!(
                    out,
                    " {:>1$}",
                    &format_duration(result[*lat_pct][*time_pct]),
                    width
                )
                .unwrap();
            }
        }
        writeln!(out, "").unwrap();
    }
}

//
// Study execution interface.
//
pub struct Studies<'a> {
    studies: Vec<&'a mut dyn Study>,
}

impl<'a> Studies<'a> {
    pub fn new() -> Self {
        Self { studies: vec![] }
    }

    pub fn add(&mut self, study: &'a mut dyn Study) -> &mut Self {
        self.studies.push(study);
        self
    }

    pub fn add_multiple(&mut self, studies: &mut Vec<&'a mut dyn Study>) -> &mut Self {
        self.studies.append(studies);
        self
    }

    pub fn run_fallible(&mut self, run: &RunCtx, start: u64, end: u64) -> Result<u64> {
        let mut nr_missed = 0;

        for (rep, _) in run.report_iter(start, end) {
            match rep {
                Ok(rep) => {
                    for study in self.studies.iter_mut() {
                        study.study(&rep)?;
                    }
                }
                Err(_) => nr_missed += 1,
            }
        }

        if start + nr_missed >= end {
            bail!("no report available between {} and {}", start, end);
        }

        Ok(nr_missed)
    }

    pub fn run(&mut self, run: &RunCtx, start: u64, end: u64) -> u64 {
        match self.run_fallible(run, start, end) {
            Ok(v) => v,
            Err(e) => {
                error!("Failed to study the reports ({})", &e);
                panic!();
            }
        }
    }
}
