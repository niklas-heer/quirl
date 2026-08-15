#[cfg(feature = "gluon")]
use gluon::ThreadExt;
#[cfg(any(feature = "lua", feature = "luau", feature = "fennel"))]
use mlua::Lua;
#[cfg(feature = "fennel")]
use mlua::{Function, Table};
#[cfg(feature = "quickjs")]
use rquickjs::{Context, Runtime};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    run()
}

#[cfg(feature = "lua")]
fn run() -> Result<(), Box<dyn std::error::Error>> {
    let lua = Lua::new();
    let value: i64 = lua.load("return 20 + 22").eval()?;
    println!("lua:{value}");
    Ok(())
}

#[cfg(feature = "luau")]
fn run() -> Result<(), Box<dyn std::error::Error>> {
    let lua = Lua::new();
    let value: i64 = lua.load("return 20 + 22").eval()?;
    println!("luau:{value}");
    Ok(())
}

#[cfg(feature = "fennel")]
fn run() -> Result<(), Box<dyn std::error::Error>> {
    let source_path = std::env::var("QUIRL_FENNEL_LUA")?;
    let source = std::fs::read_to_string(source_path)?;
    let lua = Lua::new();
    let module: Table = lua.load(source).set_name("@fennel.lua").eval()?;
    let eval: Function = module.get("eval")?;
    let value: i64 = eval.call("(+ 20 22)")?;
    println!("fennel:{value}");
    Ok(())
}

#[cfg(feature = "rhai")]
fn run() -> Result<(), Box<dyn std::error::Error>> {
    let engine = rhai::Engine::new();
    let value: i64 = engine.eval("20 + 22")?;
    println!("rhai:{value}");
    Ok(())
}

#[cfg(feature = "quickjs")]
fn run() -> Result<(), Box<dyn std::error::Error>> {
    let runtime = Runtime::new()?;
    let context = Context::full(&runtime)?;
    let value: i32 = context.with(|context| context.eval("20 + 22"))?;
    println!("quickjs:{value}");
    Ok(())
}

#[cfg(feature = "gluon")]
fn run() -> Result<(), Box<dyn std::error::Error>> {
    let vm = gluon::new_vm();
    let source = std::env::args()
        .nth(1)
        .map(std::fs::read_to_string)
        .transpose()?
        .unwrap_or_else(|| "20 + 22".to_owned());
    let (value, _) = vm.run_expr::<i64>("footprint", &source)?;
    println!("gluon:{value}");
    Ok(())
}

#[cfg(not(any(
    feature = "lua",
    feature = "luau",
    feature = "fennel",
    feature = "rhai",
    feature = "quickjs",
    feature = "gluon"
)))]
compile_error!("enable exactly one runtime feature");
