//! Dumps the RUNTIME base-cap ladders — the two hash maps the DamageInstance
//! builder walks (`FUN_1409c1cf0`, v2.0.4) — straight out of the running game.
//!
//! The static extraction (`scripts/extract-cap-tables.mjs`) reads the same
//! curves from `chara_damage_limit.tbl` / `chara_arts_damage_limit.tbl`, but
//! the runtime map is populated by the loader and nothing proves the rows
//! survive that trip unchanged. This probe reads what the builder actually
//! interpolates over, so the two can be diffed row by row.
//!
//! Run (game running, as admin): cargo run -p gbfr-logs --example cap_ladder_dump
//!
//! Layout, from the `gbfr204fast` decompile of the builder's lookup:
//!   root       = *(base + 0x7c22bc0)         (the save root)
//!   normal map = buckets root+0x2E8, mask root+0x300, end-sentinel root+0x2D8
//!   arts map   = buckets root+0x328, mask root+0x340, end-sentinel root+0x318
//!   node       = { next @ +8, key(u32) @ +0x10, rows_begin @ +0x18, rows_end @ +0x20 }
//!   rows       = vector of 8-byte POINTERS; each points at a row with
//!                x (attack rate) @ +4 and y (damage cap) @ +8

use anyhow::{bail, Context, Result};
use game_reader::MemRead;
use gbfr_logs::game_mem;

const SAVE_ROOT_RVA: u64 = 0x7c22e40;

/// (buckets, mask, end-sentinel) offsets on the save root, per map.
const MAPS: [(&str, u64, u64, u64); 2] = [
    ("normal", 0x2E8, 0x300, 0x2D8),
    ("arts", 0x328, 0x340, 0x318),
];

const ROW_SIZE: u64 = 12;
const MAX_BUCKETS: u64 = 0x10000;
const MAX_NODES_PER_BUCKET: usize = 256;
const MAX_ROWS: u64 = 4096;

fn read_u64(mem: &game_mem::Mem, addr: u64) -> Result<u64> {
    let mut buf = [0u8; 8];
    mem.read(addr, &mut buf)?;
    Ok(u64::from_le_bytes(buf))
}

fn read_u32(mem: &game_mem::Mem, addr: u64) -> Result<u32> {
    let mut buf = [0u8; 4];
    mem.read(addr, &mut buf)?;
    Ok(u32::from_le_bytes(buf))
}

fn plausible_ptr(v: u64) -> bool {
    v > 0x10000 && v < 0x0000_8000_0000_0000
}

fn main() -> Result<()> {
    let (mem, base, _exe) =
        game_mem::open_game()?.ok_or_else(|| anyhow::anyhow!("game not running"))?;

    let root = read_u64(&mem, base + SAVE_ROOT_RVA).context("read save root slot")?;
    if !plausible_ptr(root) {
        bail!("save root slot holds {root:#x}, not a pointer — wrong RVA or no save loaded");
    }
    println!("save root: {root:#x}");

    for (name, buckets_off, mask_off, end_off) in MAPS {
        let buckets = read_u64(&mem, root + buckets_off)?;
        let mask = read_u64(&mem, root + mask_off)?;
        let end = read_u64(&mem, root + end_off)?;
        println!("\n=== {name} map: buckets={buckets:#x} mask={mask:#x} end={end:#x} ===");
        if !plausible_ptr(buckets) || mask == 0 || mask >= MAX_BUCKETS {
            println!("  implausible header — dumping raw root bytes around it instead");
            let mut raw = [0u8; 0x30];
            mem.read(root + end_off, &mut raw)?;
            println!("  root+{end_off:#x}: {raw:02x?}");
            continue;
        }

        let mut seen = std::collections::BTreeMap::new();
        for bucket in 0..=mask {
            let mut node = read_u64(&mem, buckets + bucket * 8)?;
            let mut hops = 0usize;
            while plausible_ptr(node) && node != end && hops < MAX_NODES_PER_BUCKET {
                let key = read_u32(&mem, node + 0x10)?;
                if !seen.contains_key(&key) {
                    let rows_begin = read_u64(&mem, node + 0x18)?;
                    let rows_end = read_u64(&mem, node + 0x20)?;
                    seen.insert(key, (node, rows_begin, rows_end));
                }
                node = read_u64(&mem, node + 8)?;
                hops += 1;
            }
        }
        println!("  {} distinct keys", seen.len());

        for (key, (node, rows_begin, rows_end)) in &seen {
            if !plausible_ptr(*rows_begin) || rows_end < rows_begin {
                println!(
                    "  key {key:08x} node {node:#x}: bad row vector {rows_begin:#x}..{rows_end:#x}"
                );
                continue;
            }
            let bytes = rows_end - rows_begin;
            let rows = bytes / 8;
            let remainder = bytes % 8;
            print!("  key {key:08x} rows {rows} (rem {remainder})");
            if rows == 0 || rows > MAX_ROWS {
                println!(" — skipped");
                continue;
            }
            let mut ptrs = vec![0u8; bytes as usize];
            mem.read(*rows_begin, &mut ptrs)?;
            println!();
            for r in 0..rows as usize {
                let p = u64::from_le_bytes(ptrs[r * 8..r * 8 + 8].try_into().unwrap());
                if !plausible_ptr(p) {
                    println!("    [{r:3}] bad ptr {p:#x}");
                    continue;
                }
                let mut row = [0u8; ROW_SIZE as usize];
                mem.read(p, &mut row)?;
                let a = u32::from_le_bytes(row[0..4].try_into().unwrap());
                let x = f32::from_le_bytes(row[4..8].try_into().unwrap());
                let y = f32::from_le_bytes(row[8..12].try_into().unwrap());
                println!("    [{r:3}] +0={a:#010x} x={x:<8} y={y}");
            }
        }
    }

    Ok(())
}
