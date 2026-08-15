use mlua::{Function, Lua};
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
        black_box(Lua::new());
        Ok(())
    })?);

    let eval_lua = Lua::new();
    measurements.push(measure("expression_eval", EVAL_SAMPLES, || {
        black_box(
            eval_lua
                .load("return 20 + 22")
                .eval::<i64>()
                .map_err(|error| error.to_string())?,
        );
        Ok(())
    })?);

    let (host_lua, bench) = host()?;
    measurements.push(measure("warm_host_call", WARM_SAMPLES, || {
        black_box(bench.call::<i64>(41).map_err(|error| error.to_string())?);
        Ok(())
    })?);
    black_box(host_lua);

    let report = Report {
        schema_version: 1,
        runtime: "luau",
        samples: SampleCounts {
            cold_start: COLD_SAMPLES,
            expression_eval: EVAL_SAMPLES,
            warm_host_call: WARM_SAMPLES,
        },
        measurements,
        note: "Luau interpreter through mlua 0.12; static type checking is a separate build/check step.",
    };
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

fn host() -> Result<(Lua, Function), mlua::Error> {
    let lua = Lua::new();
    let host_add = lua.create_function(|_, value: i64| Ok(value + 1))?;
    lua.globals().set("host_add", host_add)?;
    lua.load("function bench(value: number): number\n  return host_add(value)\nend")
        .exec()?;
    let bench = lua.globals().get("bench")?;
    Ok((lua, bench))
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
        runtime: "luau",
        case,
        median_microseconds: micros(timings[timings.len() / 2]),
        p95_microseconds: micros(timings[(timings.len() * 95 / 100).min(timings.len() - 1)]),
        total_milliseconds: total.as_secs_f64() * 1_000.0,
    })
}

fn micros(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000_000.0
}
