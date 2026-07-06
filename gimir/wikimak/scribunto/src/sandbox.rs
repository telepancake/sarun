//! Sandbox setup (plan §3.3): strip io/package/dofile/loadfile/load/
//! loadstring, trim `debug` to just `traceback`, restrict
//! `collectgarbage`, drop `print`, and install a curated `os` whose
//! time/date/clock answer from the frame's τ — never the wall clock.
//! `require` and the mw.* library are installed separately (they need
//! the store/frame). Memory and instruction budgets are enforced by the
//! caller via mlua's allocator limit and instruction hook.

use mlua::Lua;

use crate::datetime;

/// Pure-Lua half of the sandbox: nil out capabilities that need no host
/// state. Runs before mw.* is installed.
const TRIM: &str = r#"
io = nil
package = nil
dofile = nil
loadfile = nil
load = nil
loadstring = nil
require = nil
newproxy = nil
print = nil
module = nil
local dt = debug and debug.traceback
debug = { traceback = dt or function(msg) return msg or "" end }
local realcg = collectgarbage
collectgarbage = function(opt)
    opt = opt or "collect"
    if opt == "count" then return realcg("count") end
    return 0
end
"#;

pub fn apply(lua: &Lua, tau_secs: i64) -> mlua::Result<()> {
    lua.load(TRIM).exec()?;

    let os = lua.create_table()?;
    os.set(
        "time",
        lua.create_function(move |_, arg: Option<mlua::Table>| match arg {
            None => Ok(tau_secs),
            Some(t) => {
                let g = |k: &str, d: i64| t.get::<Option<i64>>(k).ok().flatten().unwrap_or(d);
                Ok(datetime::unix_from_fields(
                    g("year", 1970),
                    g("month", 1) as u32,
                    g("day", 1) as u32,
                    g("hour", 12) as u32,
                    g("min", 0) as u32,
                    g("sec", 0) as u32,
                ))
            }
        })?,
    )?;
    os.set(
        "date",
        lua.create_function(move |lua, (fmt, time): (Option<String>, Option<i64>)| {
            let unix = time.unwrap_or(tau_secs);
            let mut fmt = fmt.unwrap_or_else(|| "%c".to_string());
            if let Some(stripped) = fmt.strip_prefix('!') {
                fmt = stripped.to_string();
            }
            let c = datetime::civil_from_unix(unix);
            if fmt == "*t" {
                let t = lua.create_table()?;
                t.set("year", c.year)?;
                t.set("month", c.month)?;
                t.set("day", c.day)?;
                t.set("hour", c.hour)?;
                t.set("min", c.min)?;
                t.set("sec", c.sec)?;
                t.set("wday", c.wday + 1)?; // Lua wday is 1=Sunday..7
                t.set("yday", c.yday + 1)?;
                t.set("isdst", false)?;
                return Ok(mlua::Value::Table(t));
            }
            let fmt = if fmt == "%c" { "%a %b %e %H:%M:%S %Y".to_string() } else { fmt };
            Ok(mlua::Value::String(lua.create_string(datetime::strftime(&fmt, &c))?))
        })?,
    )?;
    os.set("clock", lua.create_function(|_, ()| Ok(0.0f64))?)?;
    os.set("difftime", lua.create_function(|_, (a, b): (f64, f64)| Ok(a - b))?)?;
    os.set("getenv", lua.create_function(|_, _n: String| Ok(mlua::Value::Nil))?)?;
    lua.globals().set("os", os)?;
    Ok(())
}
