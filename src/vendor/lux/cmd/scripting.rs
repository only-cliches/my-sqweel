use bytes::BytesMut;
use std::time::Instant;

use crate::vendor::lux::resp;
use crate::vendor::lux::store::Store;

use super::{CmdResult, arg_str, parse_u64};

pub fn cmd_eval(args: &[&[u8]], _store: &Store, out: &mut BytesMut, _now: Instant) -> CmdResult {
    if args.len() < 3 {
        resp::write_error(out, "ERR wrong number of arguments for 'eval' command");
        return CmdResult::Written;
    }
    let script = arg_str(args[1]).to_string();
    let numkeys = parse_u64(args[2]).unwrap_or(0) as usize;
    let keys: Vec<Vec<u8>> = args[3..3 + numkeys.min(args.len() - 3)]
        .iter()
        .map(|k| k.to_vec())
        .collect();
    let argv: Vec<Vec<u8>> = args[3 + numkeys.min(args.len() - 3)..]
        .iter()
        .map(|a| a.to_vec())
        .collect();
    CmdResult::Eval { script, keys, argv }
}

pub fn cmd_evalsha(args: &[&[u8]], _store: &Store, out: &mut BytesMut, _now: Instant) -> CmdResult {
    if args.len() < 3 {
        resp::write_error(out, "ERR wrong number of arguments for 'evalsha' command");
        return CmdResult::Written;
    }
    let sha = arg_str(args[1]).to_lowercase();
    let numkeys = parse_u64(args[2]).unwrap_or(0) as usize;
    let keys: Vec<Vec<u8>> = args[3..3 + numkeys.min(args.len() - 3)]
        .iter()
        .map(|k| k.to_vec())
        .collect();
    let argv: Vec<Vec<u8>> = args[3 + numkeys.min(args.len() - 3)..]
        .iter()
        .map(|a| a.to_vec())
        .collect();
    CmdResult::Eval {
        script: format!("__SHA:{}", sha),
        keys,
        argv,
    }
}

pub fn cmd_script(args: &[&[u8]], _store: &Store, out: &mut BytesMut, _now: Instant) -> CmdResult {
    if args.len() < 2 {
        resp::write_error(out, "ERR wrong number of arguments for 'script' command");
        return CmdResult::Written;
    }
    CmdResult::ScriptOp
}
