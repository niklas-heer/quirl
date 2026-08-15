use rquickjs::{function::Func, Context, Function, Runtime};
use serde::Serialize;
use std::{
    hint::black_box,
    time::{Duration, Instant},
};

const COLD_SAMPLES: usize = 40;
const EVAL_SAMPLES: usize = 400;
const WARM_SAMPLES: usize = 10_000;

#[derive(Serialize)]
struct Report {
    schema_version: u32,
    runtime: &'static str,
    samples: SampleCounts,
    measurements: Vec<Measurement>,
    note: &'static str,
}

#[derive(Serialize)]
struct SampleCounts {
    cold_start: usize,
    expression_eval: usize,
    warm_host_call: usize,
}

#[derive(Serialize)]
struct Measurement {
    runtime: &'static str,
    case: &'static str,
    median_microseconds: f64,
    p95_microseconds: f64,
    total_milliseconds: f64,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut measurements = Vec::new();
    measurements.push(measure("cold_start", COLD_SAMPLES, || {
        let runtime = Runtime::new().map_err(|error| error.to_string())?;
        let context = Context::full(&runtime).map_err(|error| error.to_string())?;
        black_box(context);
        black_box(runtime);
        Ok(())
    })?);

    let eval_runtime = Runtime::new()?;
    let eval_context = Context::full(&eval_runtime)?;
    measurements.push(measure("expression_eval", EVAL_SAMPLES, || {
        eval_context.with(|context| {
            black_box(
                context
                    .eval::<i32, _>("20 + 22")
                    .map_err(|error| error.to_string())?,
            );
            Ok(())
        })
    })?);

    let host_runtime = Runtime::new()?;
    let host_context = Context::full(&host_runtime)?;
    let host_measurement = host_context.with(|context| -> Result<_, String> {
        context
            .globals()
            .set("hostAdd", Func::from(|value: i32| value + 1))
            .map_err(|error| error.to_string())?;
        context
            .eval::<(), _>("function bench(value) { return hostAdd(value); }")
            .map_err(|error| error.to_string())?;
        let bench: Function = context
            .globals()
            .get("bench")
            .map_err(|error| error.to_string())?;
        measure("warm_host_call", WARM_SAMPLES, || {
            black_box(
                bench
                    .call::<(i32,), i32>((41,))
                    .map_err(|error| error.to_string())?,
            );
            Ok(())
        })
    })?;
    measurements.push(host_measurement);

    let report = Report {
        schema_version: 1,
        runtime: "quickjs",
        samples: SampleCounts {
            cold_start: COLD_SAMPLES,
            expression_eval: EVAL_SAMPLES,
            warm_host_call: WARM_SAMPLES,
        },
        measurements,
        note: "QuickJS through rquickjs 0.12.2; TypeScript is checked and emitted separately before JavaScript execution.",
    };
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

fn measure(
    case: &'static str,
    samples: usize,
    mut operation: impl FnMut() -> Result<(), String>,
) -> Result<Measurement, String> {
    let total_start = Instant::now();
    let mut timings = Vec::with_capacity(samples);
    for _ in 0..samples {
        let start = Instant::now();
        operation()?;
        timings.push(start.elapsed());
    }
    let total = total_start.elapsed();
    timings.sort_unstable();
    Ok(Measurement {
        runtime: "quickjs",
        case,
        median_microseconds: micros(timings[timings.len() / 2]),
        p95_microseconds: micros(timings[(timings.len() * 95 / 100).min(timings.len() - 1)]),
        total_milliseconds: total.as_secs_f64() * 1_000.0,
    })
}

fn micros(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000_000.0
}
