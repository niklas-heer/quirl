//! Reproducible research benchmarks and release measurements for Quirl.

use mlua::{Function, Lua, Table};
use serde::Serialize;
use std::{
    fs,
    hint::black_box,
    path::PathBuf,
    time::{Duration, Instant},
};

mod preview;

const COLD_SAMPLES: usize = 40;
const EVAL_SAMPLES: usize = 400;
const WARM_SAMPLES: usize = 10_000;

#[derive(Debug, Serialize)]
struct Report {
    schema_version: u32,
    note: &'static str,
    fennel: Option<FennelMetadata>,
    samples: SampleCounts,
    measurements: Vec<Measurement>,
}

#[derive(Debug, Serialize)]
struct FennelMetadata {
    version: String,
    source: String,
}

#[derive(Debug, Serialize)]
struct SampleCounts {
    cold_start: usize,
    expression_eval: usize,
    warm_host_call: usize,
}

#[derive(Debug, Serialize)]
struct Measurement {
    runtime: &'static str,
    case: &'static str,
    median_microseconds: f64,
    p95_microseconds: f64,
    total_milliseconds: f64,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    match std::env::args().nth(1).as_deref() {
        Some("preview") => return preview::run(false),
        Some("release") => return preview::run(true),
        _ => {}
    }
    let fennel_path = argument_value("--fennel")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("QUIRL_FENNEL_LUA").map(PathBuf::from));
    let fennel_source = fennel_path.as_deref().map(fs::read_to_string).transpose()?;
    let mut measurements = Vec::new();

    measurements.push(measure("lua", "cold_start", COLD_SAMPLES, || {
        black_box(Lua::new());
        Ok(())
    })?);
    let lua_eval = Lua::new();
    measurements.push(measure("lua", "expression_eval", EVAL_SAMPLES, || {
        black_box(
            lua_eval
                .load("return 20 + 22")
                .eval::<i64>()
                .map_err(|error| error.to_string())?,
        );
        Ok(())
    })?);
    let (lua_host, lua_bench) = lua_host()?;
    measurements.push(measure("lua", "warm_host_call", WARM_SAMPLES, || {
        black_box(
            lua_bench
                .call::<i64>(41)
                .map_err(|error| error.to_string())?,
        );
        Ok(())
    })?);
    black_box(lua_host);
    let fennel =
        if let (Some(path), Some(source)) = (fennel_path.as_deref(), fennel_source.as_deref()) {
            let probe = Lua::new();
            let module = load_fennel(&probe, source)?;
            let version = module.get::<String>("version")?;

            measurements.push(measure("fennel", "compiler_load", COLD_SAMPLES, || {
                let lua = Lua::new();
                black_box(load_fennel(&lua, source).map_err(|error| error.to_string())?);
                Ok(())
            })?);

            let fennel_eval_lua = Lua::new();
            let fennel_eval_module = load_fennel(&fennel_eval_lua, source)?;
            let fennel_eval: Function = fennel_eval_module.get("eval")?;
            measurements.push(measure("fennel", "compile_and_eval", EVAL_SAMPLES, || {
                black_box(
                    fennel_eval
                        .call::<i64>("(+ 20 22)")
                        .map_err(|error| error.to_string())?,
                );
                Ok(())
            })?);

            let (fennel_host_lua, fennel_bench) = fennel_host(source)?;
            measurements.push(measure("fennel", "warm_host_call", WARM_SAMPLES, || {
                black_box(
                    fennel_bench
                        .call::<i64>(41)
                        .map_err(|error| error.to_string())?,
                );
                Ok(())
            })?);
            black_box(fennel_host_lua);

            Some(FennelMetadata {
                version,
                source: path.display().to_string(),
            })
        } else {
            None
        };

    let report = Report {
        schema_version: 1,
        note: "Microbenchmark only; compare release builds on the same idle machine. Fennel cached code runs on Lua. Static-analysis quality is evaluated separately.",
        fennel,
        samples: SampleCounts {
            cold_start: COLD_SAMPLES,
            expression_eval: EVAL_SAMPLES,
            warm_host_call: WARM_SAMPLES,
        },
        measurements,
    };

    if std::env::args().any(|argument| argument == "--json") {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("Quirl language spike (release build)\n");
        println!(
            "{:<8}  {:<18}  {:>12}  {:>12}",
            "runtime", "case", "median µs", "p95 µs"
        );
        for measurement in &report.measurements {
            println!(
                "{:<8}  {:<18}  {:>12.3}  {:>12.3}",
                measurement.runtime,
                measurement.case,
                measurement.median_microseconds,
                measurement.p95_microseconds,
            );
        }
        println!("\n{}", report.note);
    }
    Ok(())
}

#[allow(
    clippy::arithmetic_side_effects,
    reason = "the benchmark instruction limit is a small compile-time constant"
)]
fn lua_host() -> Result<(Lua, Function), mlua::Error> {
    let lua = Lua::new();
    let host_add = lua.create_function(|_, value: i64| Ok(value + 1))?;
    lua.globals().set("host_add", host_add)?;
    lua.load("function bench(value) return host_add(value) end")
        .exec()?;
    let bench = lua.globals().get("bench")?;
    Ok((lua, bench))
}

fn load_fennel(lua: &Lua, source: &str) -> Result<Table, mlua::Error> {
    lua.load(source).set_name("@fennel.lua").eval()
}

#[allow(
    clippy::arithmetic_side_effects,
    reason = "the benchmark instruction limit is a small compile-time constant"
)]
fn fennel_host(source: &str) -> Result<(Lua, Function), mlua::Error> {
    let lua = Lua::new();
    let host_add = lua.create_function(|_, value: i64| Ok(value + 1))?;
    lua.globals().set("host_add", host_add)?;
    let fennel = load_fennel(&lua, source)?;
    let compile: Function = fennel.get("compileString")?;
    let lua_source: String = compile.call("(fn [value] (_G.host_add value))")?;
    let bench = lua.load(lua_source).eval()?;
    Ok((lua, bench))
}

fn argument_value(name: &str) -> Option<String> {
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        if argument == name {
            return arguments.next();
        }
    }
    None
}

#[allow(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    reason = "the non-empty timing vector is sorted before bounded percentile indexing and averaging"
)]
fn measure(
    runtime: &'static str,
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
        runtime,
        case,
        median_microseconds: micros(timings[timings.len() / 2]),
        p95_microseconds: micros(timings[(timings.len() * 95 / 100).min(timings.len() - 1)]),
        total_milliseconds: total.as_secs_f64() * 1_000.0,
    })
}

fn micros(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000_000.0
}
